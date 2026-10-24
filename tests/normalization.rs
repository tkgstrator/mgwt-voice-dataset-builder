use mgwt_voice_dataset_builder::normalize::{normalize_transcription, normalize_voice_id};

#[test]
fn voice_ids_only_strip_wav_and_keep_case() {
    assert_eq!(
        normalize_voice_id("Scene_Coco001.WAV").unwrap(),
        "Scene_Coco001"
    );
    assert!(normalize_voice_id("../Scene_Coco001").is_err());
}

#[test]
fn spoken_text_uses_ruby_reading_and_breaks() {
    let result = normalize_transcription("<ruby=\"よみ\">漢字</ruby><br/> 架空。\r\n");
    assert_eq!(result.text, "よみ 架空。");
    assert!(result.is_lexical);
    assert!(!normalize_transcription("<unknown>架空</unknown>").is_lexical);
    assert!(!normalize_transcription("\u{0000}架空").is_lexical);
}
