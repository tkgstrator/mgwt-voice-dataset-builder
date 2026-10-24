use mgwt_voice_dataset_builder::{
    config::checked_in_approval, inventory::inventory_app, paths::ValidatedAppPath,
};

#[test]
#[ignore = "requires MGWT_APP_PATH to an authorized local app"]
fn approved_real_inventory_is_stable_across_checkpoints() {
    let path = std::env::var_os("MGWT_APP_PATH").expect("MGWT_APP_PATH required");
    let app = ValidatedAppPath::new(std::path::Path::new(&path)).unwrap();
    let approval = checked_in_approval().unwrap();
    let pre = inventory_app(&app, &approval.manifest).unwrap();
    let post = inventory_app(&app, &approval.manifest).unwrap();
    assert_eq!(pre, post);
    assert_eq!(pre.entries.len(), 62);
    assert_eq!(
        pre.entries.iter().map(|e| e.size).sum::<u64>(),
        4_603_886_665
    );
}
