//! Regression test for the taffy CSS Grid `MinContent` cache collision bug:
//!
//! In Markdown tables (`minmax(min-content, Nfr)` columns with `auto` rows),
//! when a cell contains a long unbreakable token (e.g. an inline-code identifier)
//! alongside CJK / breakable words AND that token determines the column's final
//! width, taffy's `Cache::get` used to reuse the earlier `AvailableSpace::MinContent`
//! probe (where every word / CJK character sits on its own line) as a cache hit for
//! `known_dimensions.width == Some(cached_size.width)`. That inflated the row height
//! by 2x–3x and left a huge blank void beneath the rendered lines.

use std::path::PathBuf;
use std::time::Duration;

use zgx_script::{ScriptHost, Value, testing::TestApp};

fn ui_dir() -> PathBuf {
    std::env::var_os("ZERON_ZGX_TEST_UI")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui"))
}

#[test]
fn markdown_table_row_does_not_inflate_from_min_content_cache() {
    let md = concat!(
        "| # | 事实 | 证据 |\n",
        "| --- | --- | --- |\n",
        "| 1 | agy 1.5.x 把登录存进了 Windows 凭据管理器，不再写文件 | ",
        "`LegacyGeneric:target=gemini:antigravity`，信封是 ",
        "`{token: {access_token,refresh_token,…}, auth_method:\"consumer\", id_token_email}`，",
        "没有 `client_id`/`client_secret` |\n",
        "| 2 | 1.5.14 二进制里 `acp_token` / `antigravity-acp` / `AGY_ACP_FORCE_FILE_STORAGE` 出现次数全是 0 | ",
        "Zeron 期待的那套文件存储，上游已经删掉了，只剩 `cache/acp-auth-state.json`（内容仅 `{\"methodId\":\"oauth-personal\"}`） |\n",
        "| 3 | Zeron 在 Windows 上只认那个文件 | ",
        "`agent_accounts.rs:4810` `#[cfg(not(target_os=\"macos\"))] ... -> bool { false }`：钥匙串检查被编译掉了；`:4765` `antigravity_file_login()` 要求 `refresh_token` + `client_id` |\n",
    );

    let mut app = TestApp::new(|runtime| ScriptHost::new(runtime, ui_dir().join("chat_fixture.luau")));
    app.ui.scene.borrow_mut().resize(820.0, 900.0);
    app.frames(2);
    assert!(app.host.run_command("go_s2"));
    app.frames(20);
    std::thread::sleep(Duration::from_millis(320));
    assert!(app.host.run_command("start_block_reply"));
    app.frames(3);

    let words = Value::from(md);
    app.host.shared("chat.draft", Value::Nil).set(Value::record([
        ("id", Value::from("live")),
        ("text", words.clone()),
        ("part", Value::from("t")),
        ("part_text", words),
    ]));
    app.frames(10);
    assert_eq!(app.error(), None);

    // Check across narrow (scrolling), exact min-content boundary (820px),
    // and wide (fr-expanded) window widths.
    for width in [600.0_f32, 740.0, 820.0, 960.0, 1160.0, 820.0] {
        app.ui.scene.borrow_mut().resize(width, 900.0);
        app.frames(4);
        assert_eq!(app.error(), None);

        let grid = app
            .find(".md-table-grid")
            .unwrap_or_else(|| panic!("no .md-table-grid at width={width}:\n{}", app.tree()));
        let scene = app.ui.scene.borrow();
        let cells = scene.children(grid);
        assert_eq!(cells.len(), 12, "header + 3 rows x 3 cols");

        for row in 1..=3 {
            let base = row * 3;
            let row_h = scene.bounds(cells[base + 2]).height;
            let max_text_h = (0..3)
                .map(|c| scene.bounds(scene.children(cells[base + c])[0]).height)
                .fold(0.0_f32, f32::max);

            // Cell vertical padding + bottom border is 16–17px; without the taffy
            // MinContent cache bug, no row may carry extra vertical void.
            assert!(
                row_h <= max_text_h + 24.0,
                "width={width} row {row} inflated to {row_h} px while tallest cell text is \
                 only {max_text_h} px (extra blank = {} px)",
                row_h - max_text_h
            );
        }
    }
}
