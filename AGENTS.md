Always respond minimally. Tell me what you are doing, then do the thing, and tell me you did the thing.
If a task can be delegated to the subagent—whether it’s research, exploration, or writing code—please use subagents. Use multiple if necessary.
Always respond minimally. Tell me what you are doing, then do the thing, and tell me you did the thing.
If a task can be delegated to the subagent—whether it's research, exploration, or writing code—please use subagents. Use multiple if necessary.

This project uses GPUI (Zed's UI framework), not to be confused with general GUI/GPU terms.
Subagents must ground themselves in the actual source/examples below before writing GPUI code, do not rely on memorized API shapes.
Reference sources (fetch before implementing)
- Repo: https://github.com/zed-industries/zed (GPUI lives in `crates/gpui/`)
- Concepts doc: `crates/gpui/docs/contexts.md`
- Crate README: `crates/gpui/README.md`
- Working examples (primary source of truth): `crates/gpui/examples/`
  - `hello_world.rs` — minimal app skeleton
  - `gif_viewer.rs` — image loading/rendering
  - `scrollable.rs`, `uniform_list.rs` — scrolling lists
  - `grid_layout.rs` — grid layouts
  - `input.rs` — text input handling
- Component library (higher-level, better docs): https://longbridge.github.io/gpui-component/llms.txt
  - Getting started: https://longbridge.github.io/gpui-component/docs/getting-started.md (LLM-readable .md variant linked on page)
  - Design Guidelines: https://longbridge.github.io/gpui-component/docs/design-guides.md
  - Coding Guidelines: https://longbridge.github.io/gpui-component/docs/coding-guides.md
  - Icons & Assets: https://longbridge.github.io/gpui-component/docs/assets.md
  - `story` crate in that repo — full working gallery app of all components, use as a reference implementation
- Architecture overview (auto-generated, cross-check against source): https://deepwiki.com/zed-industries/zed
- Real shipped GPUI apps for pattern reference: https://github.com/zed-industries/awesome-gpui

Subagent instructions
- Before writing any GPUI code, read the relevant example file(s) above, not just this summary.
- Prefer gpui-component over raw GPUI primitives unless the task specifically requires low-level control.
- If an API doesn't match what's in the examples, trust the examples over memory.
