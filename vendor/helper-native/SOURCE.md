# helper-native（vendored）

来源：https://github.com/Prometheus-first/helper

- 上游提交：`fd4960c4a1c3ead1dd58cb1ae33b878f2c783574`
- 本地修改：`native/moments_interaction_bridge.h`、`native/moments_interaction_call.h`
- 许可：`LICENSE`（LicenseRef-helper-noncommercial-1.0）；第三方声明见 `THIRD_PARTY_NOTICES.md`。
  仅允许个人非商业使用/分发；分发时必须保留本许可与第三方声明。

只纳入 `text_bridge.dll` 所需的 C++ 源码与 MinHook 1.3.4；构建脚本见 `tools/build-bridge.ps1`。
