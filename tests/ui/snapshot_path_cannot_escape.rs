use mgwt_voice_dataset_builder::inventory::VerifiedSnapshot;

fn escape(snapshot: &VerifiedSnapshot) {
    let _path = snapshot.with_verified_path(|path| path.to_path_buf());
}

fn main() {}
