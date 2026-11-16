# 接口适配记录

上游改了 fork 用到的公开接口时，编译会失败。每次适配记一行：上游版本、哪个接口变了、怎么改的。同步流程见 `docs/7.上游同步与发布.md`。

## 怎么记

- 一次适配一行，新的写在表格最上面。
- 「上游版本」写同步到的 tag（如 `v1.30.1`），基于 `main` 时写 `main@<短提交>`。
- 「接口」写到类型或方法名；「怎么改」写改了哪个 fork 文件、改成什么样，必要时附提交号。
- 只改了挂钩的冲突重放（见 `HOOKS.md`）不用记在这里，除非挂钩的位置或写法变了。

## 重点关注的接口

- `EditPredictionDelegate` 和 `EditPrediction`
- `SemanticsProvider`（方法增减会直接影响 Hover 装饰器）
- `Editor::register_action`、`Addon`
- `StatusItemView`
- `Settings` trait 和 `SettingsContent`

## 记录

| 上游版本 | 接口 | 怎么改 |
|---|---|---|
| `main@053d91cb14` | 无（fork 建立） | fork 分支基于上游 `main` 的 `053d91cb14`，尚未跟进过上游 |
