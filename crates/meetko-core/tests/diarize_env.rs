//! `DiarConfig::from_env` reads the process environment, so it has a test binary of its own with a
//! single test: nothing else here runs while the variables are set.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

use meetko_core::diarize::DiarConfig;
use meetko_core::Error;

#[test]
fn the_environment_decides_whether_there_is_diarization() {
    // only this test reads these variables; the data dir is a fresh one, so no model the app keeps counts
    let data = tempfile::tempdir().unwrap();
    std::env::set_var("MEETKO_DATA_DIR", data.path());
    std::env::remove_var("MEETKO_DIAR_MODEL");
    assert!(DiarConfig::from_env().unwrap().is_none(), "unset: no diarization, no error");
    std::env::set_var("MEETKO_DIAR_MODEL", "/nonexistent/diar-model");
    std::env::set_var("MEETKO_DIAR_ENGINE", "/nonexistent/engine");
    let e = DiarConfig::from_env().unwrap_err();
    assert!(matches!(e, Error::EngineNotFound(_)), "{e}");
    assert!(e.to_string().contains("MEETKO_DIAR_ENGINE"), "{e}");
    let tmp = tempfile::tempdir().unwrap();
    let engine = tmp.path().join("engine");
    std::fs::write(&engine, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("MEETKO_DIAR_ENGINE", &engine);
    let e = DiarConfig::from_env().unwrap_err();
    assert!(matches!(e, Error::ModelMissing(_)) && e.to_string().contains("MEETKO_DIAR_MODEL"), "{e}");
    let model = tmp.path().join("model");
    std::fs::create_dir(&model).unwrap();
    std::fs::write(model.join("Nemotron-3-Diarization.q8_0.gguf"), "x").unwrap();
    std::env::set_var("MEETKO_DIAR_MODEL", &model);
    assert!(matches!(DiarConfig::from_env().unwrap_err(), Error::ModelMissing(m) if m.contains("not one of the pinned")));

    // unset: the model the app keeps in the data dir is used, and checked against the pin the same way
    let managed = data.path().join("models/nemotron-3-diarization-q8_0");
    std::fs::create_dir_all(&managed).unwrap();
    std::fs::write(managed.join("Nemotron-3-Diarization.q8_0.gguf"), "y").unwrap();
    std::env::remove_var("MEETKO_DIAR_MODEL");
    assert!(matches!(DiarConfig::from_env().unwrap_err(), Error::ModelMissing(m) if m.contains("not one of the pinned")), "a swapped file is refused");
    std::env::set_var("MEETKO_DIAR_MODEL_UNPINNED", "1");
    assert!(DiarConfig::from_env().is_err(), "the development switch does not apply to the app's own folder");
    std::env::remove_var("MEETKO_DIAR_MODEL_UNPINNED");
    // the app's own model without an engine: no diarization, and no error to show on every draft
    std::env::set_var("MEETKO_DIAR_ENGINE", "/nonexistent/engine");
    assert!(DiarConfig::from_env().unwrap().is_none());
    std::env::set_var("MEETKO_DIAR_ENGINE", &engine);
    std::env::set_var("MEETKO_DIAR_MODEL", &model);
    std::env::set_var("MEETKO_DIAR_MODEL_UNPINNED", "1");
    let c = DiarConfig::from_env().unwrap().unwrap();
    assert_eq!((c.engine_bin, c.model_dir, c.unpinned, c.min_speaker_ms), (engine, model.clone(), true, 10_000), "10 s unless told otherwise");
    std::env::set_var("MEETKO_DIAR_MIN_SPEAKER_SECS", "3");
    assert_eq!(DiarConfig::from_env().unwrap().unwrap().min_speaker_ms, 3000);
    std::env::set_var("MEETKO_DIAR_MIN_SPEAKER_SECS", "junk");
    assert_eq!(DiarConfig::from_env().unwrap().unwrap().min_speaker_ms, 10_000, "junk is the default");
    std::env::set_var("MEETKO_DIAR_MIN_SPEAKER_SECS", "0");
    assert_eq!(DiarConfig::from_env().unwrap().unwrap().min_speaker_ms, 0, "0 turns the rule off");
    assert_eq!(DiarConfig::from_env().unwrap().unwrap().model_dir, model, "the environment wins over the app's model");
    for k in ["MEETKO_DIAR_MODEL", "MEETKO_DIAR_ENGINE", "MEETKO_DIAR_MODEL_UNPINNED", "MEETKO_DIAR_MIN_SPEAKER_SECS", "MEETKO_DATA_DIR"] {
        std::env::remove_var(k);
    }
}
