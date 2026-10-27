use std::collections::BTreeMap;

use crate::error::{CoreError, Result};
use crate::model::WalInfo;

const WAL_HEADER_SIZE: usize = 32;
const FRAME_HEADER_SIZE: usize = 24;
const WAL_VERSION: u32 = 3_007_000;

pub type PageVerifier<'a> = &'a dyn Fn(&[u8], u32) -> bool;

pub fn merge_wal_snapshot(
    database: &[u8],
    wal: &[u8],
    page_size: usize,
    verify_page: Option<PageVerifier<'_>>,
) -> Result<(Vec<u8>, WalInfo)> {
    let mut info = WalInfo {
        wal_bytes: wal.len(),
        database_pages: database.len() / page_size.max(1),
        ..WalInfo::default()
    };
    if wal.is_empty() {
        return Ok((database.to_vec(), info));
    }
    if page_size == 0 {
        return Err(CoreError::Wal("WAL page size must be non-zero".into()));
    }
    if wal.len() < WAL_HEADER_SIZE {
        return Err(CoreError::Wal("WAL 文件头不完整，请退出微信后重试".into()));
    }

    let magic = u32::from_be_bytes(wal[0..4].try_into().expect("4 bytes"));
    let version = u32::from_be_bytes(wal[4..8].try_into().expect("4 bytes"));
    let wal_page_size = u32::from_be_bytes(wal[8..12].try_into().expect("4 bytes")) as usize;
    if !matches!(magic, 0x377f_0682 | 0x377f_0683) || version != WAL_VERSION {
        return Err(CoreError::Wal("WAL 文件头或版本无效".into()));
    }
    if wal_page_size != page_size || database.len() % page_size != 0 {
        return Err(CoreError::Wal(
            "WAL 与数据库页大小不匹配，或主数据库页不完整".into(),
        ));
    }

    let big_endian = magic == 0x377f_0683;
    let mut state = checksum(&wal[..24], big_endian, (0, 0))?;
    let stored_header = (
        u32::from_be_bytes(wal[24..28].try_into().expect("4 bytes")),
        u32::from_be_bytes(wal[28..32].try_into().expect("4 bytes")),
    );
    if state != stored_header {
        return Err(CoreError::Wal("WAL 文件头校验失败".into()));
    }

    let salt = &wal[16..24];
    let frame_size = FRAME_HEADER_SIZE + page_size;
    let mut pending: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let mut committed: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let mut base_pages = database.len() / page_size;
    let mut size = base_pages;
    let mut commit_end = WAL_HEADER_SIZE;

    let mut offset = WAL_HEADER_SIZE;
    while offset + frame_size <= wal.len() {
        let header = &wal[offset..offset + FRAME_HEADER_SIZE];
        if &header[8..16] != salt {
            break;
        }
        let pgno = u32::from_be_bytes(header[0..4].try_into().expect("4 bytes"));
        let db_size = u32::from_be_bytes(header[4..8].try_into().expect("4 bytes")) as usize;
        if pgno == 0 || pgno > 0xffff_fffe {
            return Err(CoreError::Wal("WAL 页号无效".into()));
        }
        let page = &wal[offset + FRAME_HEADER_SIZE..offset + frame_size];
        let frame_state = checksum(&header[..8], big_endian, state)?;
        state = checksum(page, big_endian, frame_state)?;
        let stored_frame = (
            u32::from_be_bytes(header[16..20].try_into().expect("4 bytes")),
            u32::from_be_bytes(header[20..24].try_into().expect("4 bytes")),
        );
        if state != stored_frame {
            return Err(CoreError::Wal(
                "WAL 帧校验失败，请重新获取稳定的数据库副本".into(),
            ));
        }

        pending.insert(pgno, page.to_vec());
        if db_size != 0 {
            committed.extend(std::mem::take(&mut pending));
            committed.retain(|page_number, _| *page_number as usize <= db_size);
            base_pages = base_pages.min(db_size);
            let extension = committed
                .keys()
                .filter(|page_number| **page_number as usize > base_pages)
                .count();
            if db_size - base_pages != extension {
                return Err(CoreError::Wal("WAL 提交缺少数据库扩展页".into()));
            }
            size = db_size;
            commit_end = offset + frame_size;
            info.committed_frames = (commit_end - WAL_HEADER_SIZE) / frame_size;
        }
        offset += frame_size;
    }

    info.applied_pages = committed.len();
    info.ignored_tail_bytes = wal.len().saturating_sub(commit_end);
    info.database_pages = size;
    if info.committed_frames == 0 {
        return Ok((database.to_vec(), info));
    }

    let mut output = Vec::with_capacity(size * page_size);
    let base_len = (base_pages * page_size).min(database.len());
    output.extend_from_slice(&database[..base_len]);
    output.resize(size * page_size, 0);
    for (pgno, page) in committed {
        if let Some(verify) = verify_page {
            if !verify(&page, pgno) {
                return Err(CoreError::Wal(format!("WAL 第 {pgno} 页 HMAC 校验失败")));
            }
        }
        let start = (pgno as usize - 1) * page_size;
        output[start..start + page_size].copy_from_slice(&page);
    }
    Ok((output, info))
}

fn checksum(data: &[u8], big_endian: bool, state: (u32, u32)) -> Result<(u32, u32)> {
    if data.len() % 8 != 0 {
        return Err(CoreError::Wal(
            "WAL checksum input is not 8-byte aligned".into(),
        ));
    }
    let mut a = state.0;
    let mut b = state.1;
    for chunk in data.chunks_exact(8) {
        let (x, y) = if big_endian {
            (
                u32::from_be_bytes(chunk[0..4].try_into().expect("4 bytes")),
                u32::from_be_bytes(chunk[4..8].try_into().expect("4 bytes")),
            )
        } else {
            (
                u32::from_le_bytes(chunk[0..4].try_into().expect("4 bytes")),
                u32::from_le_bytes(chunk[4..8].try_into().expect("4 bytes")),
            )
        };
        a = a.wrapping_add(x).wrapping_add(b);
        b = b.wrapping_add(y).wrapping_add(a);
    }
    Ok((a, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_checksum(mut state: (u32, u32), header8: &[u8], page: &[u8], big: bool) -> (u32, u32) {
        state = checksum(header8, big, state).unwrap();
        checksum(page, big, state).unwrap()
    }

    #[test]
    fn merges_one_committed_frame() {
        let page_size = 4096;
        let database = vec![0u8; page_size * 2];
        let mut page = vec![0xABu8; page_size];
        page[0] = 0x5A;

        let mut wal = vec![0u8; 32];
        wal[0..4].copy_from_slice(&0x377f_0682u32.to_be_bytes());
        wal[4..8].copy_from_slice(&WAL_VERSION.to_be_bytes());
        wal[8..12].copy_from_slice(&(page_size as u32).to_be_bytes());
        wal[12..16].copy_from_slice(&1u32.to_be_bytes());
        wal[16..24].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let header_state = checksum(&wal[..24], false, (0, 0)).unwrap();
        wal[24..28].copy_from_slice(&header_state.0.to_be_bytes());
        wal[28..32].copy_from_slice(&header_state.1.to_be_bytes());

        let mut header = vec![0u8; 24];
        header[0..4].copy_from_slice(&1u32.to_be_bytes());
        header[4..8].copy_from_slice(&2u32.to_be_bytes());
        header[8..16].copy_from_slice(&wal[16..24]);
        let state = frame_checksum(header_state, &header[..8], &page, false);
        header[16..20].copy_from_slice(&state.0.to_be_bytes());
        header[20..24].copy_from_slice(&state.1.to_be_bytes());
        wal.extend_from_slice(&header);
        wal.extend_from_slice(&page);

        let (output, info) = merge_wal_snapshot(&database, &wal, page_size, None).unwrap();
        assert_eq!(info.committed_frames, 1);
        assert_eq!(info.database_pages, 2);
        assert_eq!(&output[..page_size], page.as_slice());
        assert_eq!(output.len(), page_size * 2);
    }
}
