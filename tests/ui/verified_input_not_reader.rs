use mgwt_voice_dataset_builder::inventory::VerifiedInputFile;
use std::io::Read;

fn must_not_compile(file: &VerifiedInputFile) {
    let _: &dyn Read = file;
    let _ = file.try_clone_file();
}

fn main() {}
