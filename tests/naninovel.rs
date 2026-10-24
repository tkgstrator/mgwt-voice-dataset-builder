use mgwt_voice_dataset_builder::{model::RawScriptObject, naninovel::parse_script_object};
use unity_rs_core::type_tree::{TypeField, TypeValue as V};

fn obj(fields: Vec<(&str, V)>) -> V {
    V::Object(
        fields
            .into_iter()
            .map(|(name, value)| TypeField {
                name: name.into(),
                value,
            })
            .collect(),
    )
}
fn s(value: &str) -> V {
    V::String(value.into())
}
fn arr(values: Vec<V>) -> V {
    V::Array(values)
}
fn fixture() -> RawScriptObject {
    let command = |rid: &str| obj(vec![("rid", s(rid))]);
    let entry = |rid: &str, kind: &str, data: V| {
        obj(vec![
            ("rid", s(rid)),
            ("type", obj(vec![("class", s(kind))])),
            ("data", data),
        ])
    };
    let voice = obj(vec![
        (
            "Text",
            obj(vec![(
                "value",
                obj(vec![(
                    "parts",
                    arr(vec![obj(vec![
                        ("id", s("Scene_Coco001")),
                        ("spot", V::Signed(0)),
                        ("text", s("")),
                    ])]),
                )]),
            )]),
        ),
        ("AuthorId", obj(vec![("value", s("Coco"))])),
    ]);
    RawScriptObject {
        bundle_path: "invented.bundle".into(),
        file_index: 0,
        object_index: 0,
        path_id: 7,
        name: None,
        container: Some("scripts/demo".into()),
        value: obj(vec![
            ("path", s("scripts/demo")),
            (
                "playlist",
                obj(vec![(
                    "commands",
                    arr(vec![command("a"), command("b"), command("c")]),
                )]),
            ),
            (
                "references",
                obj(vec![(
                    "RefIds",
                    arr(vec![
                        entry("a", "Wait", obj(vec![])),
                        entry("b", "PrintTextModified", voice),
                        entry(
                            "c",
                            "PlayVoice",
                            obj(vec![(
                                "VoicePath",
                                obj(vec![("value", s("Scene_Coco002"))]),
                            )]),
                        ),
                    ]),
                )]),
            ),
            (
                "textMap",
                obj(vec![(
                    "idToText",
                    obj(vec![
                        ("keys", arr(vec![s("Scene_Coco001")])),
                        ("values", arr(vec![s("架空の台詞。")])),
                    ]),
                )]),
            ),
        ]),
    }
}

#[test]
fn embedded_reference_and_voice_rejection_account_for_commands() {
    let result = parse_script_object(&fixture()).unwrap();
    assert_eq!(result.command_count, 3);
    assert_eq!(result.parsed_references.len(), 1);
    assert_eq!(result.parsed_references[0].voice_id, "Scene_Coco001");
    assert_eq!(result.parsed_references[0].neighbor_commands.len(), 3);
    assert_eq!(result.candidate_rejections.len(), 1);
    assert_eq!(
        result.candidate_rejections[0].reason,
        "unsupported_asset_shape"
    );
    assert_eq!(
        result.candidate_rejections[0].blocking_group_ids,
        vec!["Scene_Coco002"]
    );
    assert_eq!(result.speaker_evidence.len(), 2);
}

#[test]
fn unreadable_top_level_is_fatal() {
    let mut raw = fixture();
    raw.value = obj(vec![]);
    assert!(parse_script_object(&raw).is_err());
}
