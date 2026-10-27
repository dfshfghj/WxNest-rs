use chrono::{Datelike, Local, TimeZone};

pub fn format_session_time(timestamp: Option<i64>) -> String {
    format_session_time_at(timestamp, Local::now())
}

pub fn format_session_time_at(timestamp: Option<i64>, now: chrono::DateTime<Local>) -> String {
    let Some(timestamp) = timestamp.filter(|value| *value > 0) else {
        return String::new();
    };
    let Some(value) = Local.timestamp_opt(timestamp, 0).single() else {
        return String::new();
    };

    let time_text = value.format("%H:%M").to_string();
    let today = now.date_naive();
    let target = value.date_naive();
    let day_diff = (today - target).num_days();

    if day_diff == 0 {
        return time_text;
    }
    if day_diff == 1 {
        return format!("昨天 {time_text}");
    }
    if (2..=6).contains(&day_diff) {
        let names = [
            "星期一",
            "星期二",
            "星期三",
            "星期四",
            "星期五",
            "星期六",
            "星期日",
        ];
        return format!(
            "{} {time_text}",
            names[value.weekday().num_days_from_monday() as usize]
        );
    }
    if value.year() == now.year() {
        return format!("{}月{}日 {time_text}", value.month(), value.day());
    }
    format!(
        "{}年{}月{}日 {time_text}",
        value.year(),
        value.month(),
        value.day()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn formats_today_and_yesterday() {
        let now = Local.with_ymd_and_hms(2026, 10, 27, 12, 0, 0).unwrap();
        let today = now.timestamp();
        assert_eq!(
            format_session_time_at(Some(today), now),
            now.format("%H:%M").to_string()
        );
        let yesterday = (now - chrono::Duration::days(1)).timestamp();
        assert!(format_session_time_at(Some(yesterday), now).starts_with("昨天 "));
    }

    #[test]
    fn ignores_missing_timestamp() {
        assert_eq!(format_session_time(None), "");
        assert_eq!(format_session_time(Some(0)), "");
    }
}
