#![no_main]
use freemkv::settings::Settings;
use freemkv::title_identity::TitleIdentity;
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    if data.len() > 65536 {
        return;
    }
    if let Ok(settings) = serde_json::from_slice::<Settings>(data) {
        let encoded = serde_json::to_vec(&settings).unwrap();
        let decoded: Settings = serde_json::from_slice(&encoded).unwrap();
        assert!(settings == decoded);
    }
    if let Ok(identity) = serde_json::from_slice::<TitleIdentity>(data) {
        let encoded = serde_json::to_vec(&identity).unwrap();
        assert_eq!(
            identity,
            serde_json::from_slice::<TitleIdentity>(&encoded).unwrap()
        );
    }
});
