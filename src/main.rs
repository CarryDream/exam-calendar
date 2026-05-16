use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use exam_calendar::server::{ServerOptions, serve};
use exam_calendar::{GenerationOptions, generate_all};

#[tokio::main]
async fn main() -> Result<(), exam_calendar::BoxError> {
    let args: Vec<String> = env::args().collect();

    match args.get(1).map(String::as_str) {
        Some("generate") => {
            let input_dir = args
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("data/calendars"));
            let output_dir = args
                .get(3)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("public/calendars"));

            let written = generate_all(&GenerationOptions {
                input_dir,
                output_dir,
            })?;

            for path in written {
                println!("{}", path.display());
            }
        }
        Some("serve") => {
            let addr = args
                .get(2)
                .map(|value| value.parse::<SocketAddr>())
                .transpose()?
                .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 3000)));
            serve(ServerOptions {
                db_path: PathBuf::from("work/exam-calendar.sqlite"),
                data_dir: PathBuf::from("data/calendars"),
                output_dir: PathBuf::from("public/calendars"),
                addr,
            })
            .await?;
        }
        _ => {
            eprintln!(
                "Usage:\n  exam-calendar generate [input-dir] [output-dir]\n  exam-calendar serve [addr]"
            );
            std::process::exit(2);
        }
    }

    Ok(())
}
