fn main() {
    // only these commands exist for the webview; capabilities/default.json grants each by name
    let cmds = &[
        "status", "list_meetings", "get_meeting", "get_version", "resolve_anchor", "approve",
        "transcribe_start", "live_draft_start", "transcribe_cancel", "capture_start", "capture_stop",
        "list_plugins", "plugin_pick", "plugin_add", "plugin_run", "list_results", "list_stale",
        "plugin_secrets", "plugin_secret_set", "plugin_secret_clear",
        "gallery_list", "gallery_preview", "gallery_install", "export_version", "export_save",
        "meeting_watch_get", "meeting_watch_set", "meeting_prompt_answer",
        "search", "model_setup", "model_download", "model_pick", "model_cancel", "setup_card_dismiss",
        "batch_engine_set", "auto_retranscribe_set",
        "overlay_get", "speaker_rename", "speaker_merge", "speaker_reset", "turn_edit", "turn_reset", "overlay_discard",
        "vocabulary_get", "vocabulary_set", "vocabulary_preview", "vocabulary_import", "vocabulary_export", "vocabulary_display",
    ];
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(cmds)))
        .expect("tauri build");
}
