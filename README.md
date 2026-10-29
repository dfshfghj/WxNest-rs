# WxNest-rs

让本地 Agent 住进微信作为机器人使用。

支持 Claude Code、Codex、Gemini CLI、Cursor Agent、Qoder CLI，以及任意能通过命令行调用的自建 Agent。

## 功能

每个会话对应一个工作目录，可以在群聊中指挥 Agent 的工作；

提供 MCP，示例：

```json
{
  "mcpServers": {
    "wechat": {
      "command": "D:\\wechat-mcp\\wechat-agent.exe",
      "args": ["mcp"],
      "env": {
        "WECHAT_REALTIME_ROOT": "D:\\...\\wxid_xxx\\db_storage",
        "WECHAT_KEY_STORE": "D:\\...\\account_keys.json",
        "WCE_SELF_BROKER": "D:\\wechat-mcp\\wechat-agent.exe",
        "WCE_NATIVE_BRIDGE": "D:\\wechat-mcp\\text_bridge.dll",
        "WCE_MEDIA_DIR": "D:\\wechat-mcp\\media"
      }
    }
  }
}
```

用gpui写了一套桌面端，包含聊天、朋友圈、联系人、视频号、小程序、收付款、搜索、AI 自动回复。

注意工作目录最好开一个独立的，不要给 Agent 过高的权限，防止被人哄骗而出问题。

暂时只支持 Windows，以后可能会支持 Linux （Mac 就随缘了，主要是我也没有 Mac）

## 支持版本

只读数据功能支持的比较广泛，4.x 都可用，但是发送消息这些操作功能只做了 4.1.15.13、4.1.15.56 两个版本，因为每一个小版本都需要重新定位，后面有时间再跟进；版本不匹配时操作会拒绝执行。

## 许可证

本仓库原创代码采用 **CC BY-NC-SA 4.0**（Attribution-NonCommercial-ShareAlike 4.0 International），全文见 [LICENSE](LICENSE)。仅允许非商业使用；分发修改版需保留署名并以相同许可证共享。

第三方组件不适用上述许可证：

- `vendor/helper-native` 与编译出的 `text_bridge.dll`：`LicenseRef-helper-noncommercial-1.0`，仅限个人非商业使用；分发时必须保留其 `LICENSE` 与 `THIRD_PARTY_NOTICES.md`。
- MinHook：BSD 2-Clause。
- 其余 Rust 依赖按各自 crate 的许可证。

CC BY-NC-SA 4.0 不替代、也不扩大任何第三方许可证授予的权利。
