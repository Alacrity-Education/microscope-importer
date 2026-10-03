use anyhow::Result;

use microscope_importer::app::App;
use microscope_importer::paths::Paths;
use microscope_importer::util;

const HELP: &str = "\
microscope-importer - import photos and videos from microscope SD cards

USAGE:
    microscope-importer [--help | --version]

Starts a terminal interface that lists every SD card (mounted or not).
Select one with the arrow keys and press Enter to import it; several cards
can be imported at the same time.

Photos (jpg, jpeg, png) go to ~/Pictures/Microscope. Videos (mp4) are copied
to ~/Videos/Microscope/.originalframes/<card>/ and the camera's 1-20 minute
segments are stitched losslessly into whole recordings in ~/Videos/Microscope.
Once everything is copied and verified by checksum, the imported files are
deleted from the card and it is unmounted.

ENVIRONMENT:
    MICROSCOPE_IMPORTER_VIDEOS     videos destination  (default ~/Videos/Microscope)
    MICROSCOPE_IMPORTER_PICTURES   photos destination  (default ~/Pictures/Microscope)
    MICROSCOPE_IMPORTER_LOOP=1     also offer loop devices (for testing with card images)

Log: ~/.local/state/microscope-importer/import.log
";

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("-h" | "--help") => {
            print!("{HELP}");
            return Ok(());
        }
        Some("-V" | "--version") => {
            println!("microscope-importer {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some(other) => {
            eprintln!("unknown argument '{other}'\n\n{HELP}");
            std::process::exit(2);
        }
        None => {}
    }
    let paths = Paths::from_env()?;
    util::init_log(paths.log_file());
    util::log("started");
    let mut terminal = ratatui::init();
    let result = App::new(paths).run(&mut terminal);
    ratatui::restore();
    util::log("stopped");
    result
}
