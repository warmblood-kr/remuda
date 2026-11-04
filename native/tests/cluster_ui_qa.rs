//! Independent acceptance probes through the public frontend composer seam.
use remuda_native::cluster_ui::*;
const DEFAULT: &str = include_str!("../src/cluster_ui.lua");

#[test]
fn enrichment_keeps_selected_name_and_operational_state_at_real_list_widths() {
    for cols in [40, 60, 80, 120] {
        let mut m = model(1);
        m.viewport.cols = cols;
        let session = &mut m.groups[0].sessions[0];
        session.label = "build".into();
        session.pending = 3;
        session.status_cells = text_width("live · pending input 3") as u16;
        session.display = Some(DisplayFacts {
            provider: "butler".into(),
            alias: None,
            leader: Some("team-leader".into()),
            kind: Some("codex".into()),
            model: Some("a-long-configured-model-name".into()),
            context_percent: Some(42),
            context_used: None,
            context_window: None,
        });
        let c = Composer::embedded().compose(&m).unwrap();
        let width = match c.layout.axis {
            Axis::Horizontal => usize::from(c.layout.list_cells),
            Axis::Vertical => usize::from(cols),
        };
        let out = render_row(&c.groups[0].sessions[0].rows[0], width, 1);
        assert!(out.starts_with('>'), "selection lost at {cols}: {out:?}");
        assert!(out.contains("bui"), "name lost at {cols}: {out:?}");
        assert!(
            out.contains("live"),
            "canonical state lost at {cols}: {out:?}"
        );
        assert_eq!(c.targets(), vec![2]);
    }
}
fn model(n: usize) -> Model {
    let mut m = Model::new(Surface::Cluster, Viewport::new(80, 24, 22));
    m.counts = Some(Counts {
        reachable: 1,
        total: 1,
    });
    m.selected = Some(2);
    m.groups.push(Group {
        id: 1,
        kind: GroupKind::Local,
        selected: false,
        expanded: true,
        label: "studio".into(),
        state: None,
        age_seconds: Some(0),
        pending: 0,
        status_cells: 20,
        sessions: (0..n)
            .map(|i| SessionEntry {
                display: None,
                id: i as u32 + 2,
                label: format!("会話{i}"),
                selected: i == 0,
                alive: true,
                pending: 0,
                status_cells: 4,
            })
            .collect(),
    });
    m
}
fn user(src: &str, m: &Model) -> Composition {
    Composer::with_sources(DEFAULT, Some(("qa.lua", src)))
        .compose(m)
        .expect("fallback must compose")
}
#[test]
fn default_handles_a_thousand_sessions_without_target_loss() {
    let m = model(1000);
    let c = Composer::with_sources(DEFAULT, None)
        .compose_fixture(&m)
        .unwrap();
    assert_eq!(c.source, ComposeSource::Default);
    assert_eq!(c.targets(), (2..1002).collect::<Vec<_>>());
}
#[test]
fn custom_order_colors_and_nondefault_split_are_accepted() {
    let c = user(
        r#"local base=compose
function compose(m)
 local d=base(m); local s=d.groups[1].sessions
 s[1],s[2]=s[2],s[1]
 s[1].rows[1].spans={{text='custom',fg={rgb={r=12,g=34,b=56}},bg={index=200},reverse=true},{field='status'}}
 d.layout={axis='vertical',list_cells=2,divider='-'}
 return d
end"#,
        &model(2),
    );
    assert_eq!(c.source, ComposeSource::User, "{:?}", c.diagnostic);
    assert_eq!(c.targets(), vec![3, 2]);
    assert_eq!(c.layout.list_cells, 2);
    let s = &c.groups[0].sessions[0].rows[0].spans[0];
    assert_eq!(s.fg, Some(Color::Rgb(12, 34, 56)));
    assert_eq!(s.bg, Some(Color::Index(200)));
    assert!(s.reverse);
}
#[test]
fn invalid_documents_fall_back_atomically_with_complete_targets() {
    let mutations = [
        "d.version=999",
        "d.groups={}",
        "d.groups[1].sessions[1].id=999999",
        "d.groups[1].sessions[2].id=d.groups[1].sessions[1].id",
        "d.groups[1].sessions[1].rows={}",
        "d.layout.list_cells=0",
        "d.layout.list_cells=0/0",
        "d.layout.list_cells=1.5",
        "d.layout.axis='diagonal'",
        "d.layout.divider='界'",
        "d.layout.divider=string.char(27)..'[2J'",
        "d.groups[1].sessions[1].rows[1].spans[1].fg={index=256}",
        "d.groups[1].sessions[1].rows[1].spans[1].text=string.rep('x',1025)",
        "setmetatable(d,{__index=function() error('metamethod') end})",
    ];
    for change in mutations {
        let src = format!(
            "local base=compose; function compose(m) local d=base(m); {change}; return d end"
        );
        let c = user(&src, &model(2));
        assert_eq!(c.source, ComposeSource::Default, "accepted {change}");
        assert_eq!(c.targets(), vec![2, 3], "partial fallback: {change}");
        assert!(c.diagnostic.is_some(), "missing diagnostic: {change}");
    }
}
#[test]
fn sandbox_exposes_only_display_capabilities() {
    let c = user(
        r#"local base=compose
function compose(m)
 for _,k in ipairs({'io','os','package','debug','coroutine','load','loadfile','dofile','require','print','warn','pcall','xpcall'}) do
  assert(_G[k]==nil,k..' capability exposed')
 end
 for _,k in ipairs({'find','match','gmatch','gsub'}) do
  assert(string[k]==nil,k..' table capability exposed')
  assert(('a')[k]==nil,k..' method capability exposed')
 end
 assert(cell_width('界')==2)
 return base(m)
end"#,
        &model(1),
    );
    assert_eq!(c.source, ComposeSource::User, "{:?}", c.diagnostic);
}
#[test]
fn runtime_and_syntax_failures_use_default() {
    for src in [
        "this is not lua",
        "function compose(m) error('qa failure') end",
        "function compose(m) while true do end end",
    ] {
        let c = user(src, &model(1));
        assert_eq!(c.source, ComposeSource::Default);
        assert_eq!(c.targets(), vec![2]);
        assert!(c.diagnostic.is_some());
    }
}
#[test]
fn broken_embedded_source_requests_diagnosed_emergency() {
    let err = Composer::with_sources("function compose(m) error('broken default') end", None)
        .compose(&model(1))
        .unwrap_err();
    assert_eq!(err.source, ComposeSource::Emergency);
    assert!(!err.message.is_empty());
}
#[test]
fn changed_model_never_reuses_a_cached_target_set() {
    let composer = Composer::with_sources(DEFAULT, None);
    let mut m = model(2);
    assert_eq!(composer.compose(&m).unwrap().targets(), vec![2, 3]);
    m.groups[0].sessions.remove(0);
    m.groups[0].sessions[0].id = 77;
    m.selected = Some(77);
    assert_eq!(composer.compose(&m).unwrap().targets(), vec![77]);
}
#[test]
fn default_split_boundary_and_peer_width_remain_policy() {
    for (cols, split) in [(40, false), (59, false), (60, true), (80, true)] {
        let mut m = model(1);
        m.viewport.cols = cols;
        let c = Composer::with_sources(DEFAULT, None).compose(&m).unwrap();
        assert_eq!(c.layout.preview_width(&m.viewport) < cols, split);
        assert!(c.layout.preview_body_height(&m.viewport) > 0);
    }
    let mut m = model(2);
    m.surface = Surface::Peer;
    m.groups[0].kind = GroupKind::Peer;
    m.viewport.preview_chrome_rows = 0;
    m.viewport.gutter_cells = 0;
    let c = Composer::with_sources(DEFAULT, None).compose(&m).unwrap();
    assert!((16..=40).contains(&c.layout.list_cells));
    assert_eq!(c.targets(), vec![2, 3]);
}
#[test]
fn unicode_fitting_never_exceeds_cell_budget() {
    for text in ["界界界", "e\u{301}e\u{301}", "👩‍💻hello", "abc"] {
        for cells in 0..10 {
            assert!(
                text_width(&fit_text(text, cells)) <= cells,
                "{text:?} at {cells}"
            );
        }
    }
}
#[test]
fn user_failure_keeps_default_available_for_changed_model_during_backoff() {
    let composer = Composer::with_sources(
        DEFAULT,
        Some(("qa.lua", "function compose(m) error('user failed') end")),
    );
    assert_eq!(
        composer.compose(&model(1)).unwrap().source,
        ComposeSource::Default
    );
    let c = composer
        .compose(&model(2))
        .expect("default remains usable during user backoff");
    assert_eq!(c.source, ComposeSource::Default);
    assert_eq!(c.targets(), vec![2, 3]);
}
#[test]
fn catching_an_instruction_budget_error_cannot_accept_a_document() {
    let c = user(
        "local base=compose; function compose(m) pcall(function() while true do end end); return base(m) end",
        &model(1),
    );
    assert_eq!(
        c.source,
        ComposeSource::Default,
        "caught budget must poison attempt"
    );
}
#[test]
fn shared_row_aliases_over_the_aggregate_cap_are_rejected() {
    let c = user(
        r#"local base=compose; function compose(m)
 local d=base(m); local row={spans={{text='alias'}}}; local rows={}
 for i=1,2100 do rows[i]=row end
 d.title=rows; d.empty=rows
 return d
end"#,
        &model(1),
    );
    assert_eq!(c.source, ComposeSource::Default);
    assert_eq!(c.targets(), vec![2]);
}
#[test]
fn fleet_over_row_cap_reports_emergency_instead_of_partial_success() {
    let m = model(MAX_ROWS + 1);
    let e = Composer::with_sources(DEFAULT, None)
        .compose(&m)
        .unwrap_err();
    assert_eq!(e.source, ComposeSource::Emergency);
}

#[test]
fn named_rgb_rejects_missing_fractional_and_out_of_range_channels() {
    for rgb in [
        "{r=1,g=2}",
        "{r=-1,g=2,b=3}",
        "{r=256,g=2,b=3}",
        "{r=1.5,g=2,b=3}",
    ] {
        let src = format!(
            "local base=compose; function compose(m) local d=base(m); d.groups[1].sessions[1].rows[1].spans[1].fg={{rgb={rgb}}}; return d end"
        );
        assert_eq!(
            user(&src, &model(1)).source,
            ComposeSource::Default,
            "accepted {rgb}"
        );
    }
}

// Run the hostile hook-catcher in an isolated test process so a broken sandbox
// cannot hang the entire test runner. Three seconds is a generous safety stop,
// not a performance assertion against the normal composition budget.
#[test]
fn repeated_budget_catching_child() {
    if std::env::var_os("CLUSTER_QA_HOSTILE_CHILD").is_none() {
        return;
    }
    let c = user(
        "function compose(m) while true do pcall(function() while true do end end) end end",
        &model(1),
    );
    assert_eq!(c.source, ComposeSource::Default);
}
#[test]
fn repeated_budget_catching_cannot_hang_the_frontend() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "repeated_budget_catching_child", "--nocapture"])
        .env("CLUSTER_QA_HOSTILE_CHILD", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "hostile config did not safely fall back");
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!(
                "hostile config catches budget errors indefinitely; child killed after safety deadline"
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn rendered_rows_clip_only_between_complete_graphemes() {
    use unicode_segmentation::UnicodeSegmentation;
    for text in ["👩‍💻xyz", "🇰🇷xyz", "e\u{301}e\u{301}xyz", "界xyz"] {
        let boundaries: Vec<usize> = std::iter::once(0)
            .chain(text.grapheme_indices(true).map(|(i, g)| i + g.len()))
            .collect();
        for cells in 0..7 {
            let out = render_row(&Row::plain(text), cells, 0);
            assert!(text.starts_with(&out), "unexpected rendered text {out:?}");
            assert!(
                boundaries.contains(&out.len()),
                "split grapheme {text:?} -> {out:?} at {cells}"
            );
            assert!(text_width(&out) <= cells);
        }
    }
}

#[test]
fn pathological_pattern_child() {
    if std::env::var_os("CLUSTER_QA_PATTERN_CHILD").is_none() {
        return;
    }
    let c = user(
        "local base=compose; function compose(m) string.find(string.rep('a',30),string.rep('a?',30)..string.rep('a',30)..'b'); return base(m) end",
        &model(1),
    );
    assert!(matches!(
        c.source,
        ComposeSource::Default | ComposeSource::User
    ));
}
#[test]
fn native_pattern_work_cannot_hang_the_frontend() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "pathological_pattern_child", "--nocapture"])
        .env("CLUSTER_QA_PATTERN_CHILD", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "native pattern probe failed");
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("native string pattern evades budget; child killed after safety deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn canonical_status_has_lua_owned_position_and_color() {
    let mut m = model(1);
    m.groups[0].sessions[0].pending = 7;
    let c = user(
        r#"local base=compose; function compose(m)
 local d=base(m); d.groups[1].sessions[1].rows[1].spans={
 {field='status',fg={index=201},bg={rgb={r=4,g=5,b=6}},bold=true},
 {text=' AFTER'} }; return d end"#,
        &m,
    );
    assert_eq!(c.source, ComposeSource::User, "{:?}", c.diagnostic);
    let row = &c.groups[0].sessions[0].rows[0];
    assert_eq!(row.spans[0].text, "live · pending input 7");
    assert_eq!(row.spans[0].fg, Some(Color::Index(201)));
    assert_eq!(row.spans[0].bg, Some(Color::Rgb(4, 5, 6)));
    assert_eq!(row.spans[1].text, " AFTER");
}

#[test]
fn default_long_labels_keep_operational_status_visible() {
    for cols in [40, 60, 100] {
        for label in ["x".repeat(80), "会話".repeat(40)] {
            let mut m = model(1);
            m.viewport.cols = cols;
            m.groups[0].sessions[0].label = label.clone();
            m.groups[0].sessions[0].pending = 3;
            m.groups[0].sessions[0].status_cells = text_width("live · pending input 3") as u16;
            let c = Composer::with_sources(DEFAULT, None).compose(&m).unwrap();
            let row = &c.groups[0].sessions[0].rows[0];
            let width = if c.layout.preview_width(&m.viewport) < cols {
                usize::from(c.layout.list_cells)
            } else {
                usize::from(cols)
            };
            let out = render_row(row, width, usize::from(m.viewport.gutter_cells));
            // Check content through SGR; plain default status text is contiguous.
            assert!(out.contains("live"), "live hidden at {cols}: {out:?}");
            assert!(
                out.contains(label.chars().next().unwrap()),
                "name hidden at {cols}: {out:?}"
            );
            if width >= 1 + 12 + text_width("live · pending input 3") {
                assert!(
                    out.contains("pending input 3"),
                    "fitting status hidden at {cols}: {out:?}"
                );
            }
        }
    }
}

#[test]
fn full_config_diagnostics_are_control_free_and_bounded() {
    let name = format!("qa\x1b]0;injected\x07\n{}", "x".repeat(3000));
    let composer = Composer::with_sources(DEFAULT, Some((&name, "function broken syntax")));
    let composition = composer.compose(&model(1)).unwrap();
    for message in [composer.diagnostic(), composition.diagnostic] {
        let message = message.expect("load error must be diagnosed");
        assert!(
            !message.chars().any(char::is_control),
            "diagnostic contains controls"
        );
        assert!(
            message.chars().count() <= 256,
            "diagnostic unbounded: {}",
            message.len()
        );
    }
}
