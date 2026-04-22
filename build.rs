fn main() {
    #[cfg(windows)]
    {
        // Embed a Windows application manifest declaring Win10/11 support and
        // UTF-8 as the active code page. Without this, Windows treats the
        // binary as a legacy app and applies ConPTY/console compatibility
        // shims — which on Insider Dev builds (>=26200) silently break child
        // output from `CreatePseudoConsole`. WezTerm and conhost-derived tools
        // ship an equivalent manifest. See docs/archived/HANDOVER-windows-conpty-nested.md.
        println!("cargo:rerun-if-changed=assets/windows/agend-terminal.rc");
        println!("cargo:rerun-if-changed=assets/windows/agend-terminal.manifest");
        embed_resource::compile("assets/windows/agend-terminal.rc", embed_resource::NONE);
    }
}
