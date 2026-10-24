use mgwt_voice_dataset_builder::inventory::VerifiedParserSession;

#[derive(Clone)]
struct Lazy;

fn escape(session: VerifiedParserSession<Lazy>) {
    let _escaped = session.run(|parser| Ok(parser.clone()));
}

fn main() {}
