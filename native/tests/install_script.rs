const INSTALL_SH: &str = include_str!("../../docs/install.sh");
const INSTALL_PS1: &str = include_str!("../../docs/install.ps1");

#[test]
fn both_installers_offer_the_gated_butler_setup_and_next_steps() {
    for (name, source, gate) in [
        (
            "install.sh",
            INSTALL_SH,
            r#"if [ "${REMUDA_INSTALL_BUTLER:-}" = 1 ]; then"#,
        ),
        (
            "install.ps1",
            INSTALL_PS1,
            "if ($env:REMUDA_INSTALL_BUTLER -eq '1') {",
        ),
    ] {
        assert!(
            source.contains(gate),
            "{name} is missing its Butler opt-in gate"
        );
        assert!(
            source.contains("mod install warmblood-kr/remuda-butler --force"),
            "{name} is missing its Butler mod install command"
        );
        assert!(
            source.contains("Next: remuda butler doctor"),
            "{name} is missing the successful-install next step"
        );
        assert!(
            source.contains("Next: remuda mod install warmblood-kr/remuda-butler --force"),
            "{name} is missing the failed-install next step"
        );
    }
}
