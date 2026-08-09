use outlook_pst::{UnicodePstFile, UnicodePstMessage};
use std::{env, io, path::PathBuf};

fn main() -> io::Result<()> {
    let path = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("fixture.pst"));
    UnicodePstFile::create(
        &path,
        &UnicodePstMessage {
            subject: "Memex PST export",
            sender_name: "Memex Sender",
            sender_email: "sender@example.com",
            recipient_name: "Memex Recipient",
            recipient_email: "recipient@example.com",
            body: "Hello from Memex PST export.",
            message_id: "<memex-pst-export@example.com>",
            delivery_time: 133_750_080_000_000_000,
        },
    )?;
    println!("{}", path.display());
    Ok(())
}
