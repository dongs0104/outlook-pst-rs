use outlook_pst::{UnicodePstAttachment, UnicodePstFile, UnicodePstMessage};
use std::{env, io, path::PathBuf};

fn main() -> io::Result<()> {
    let mut args = env::args_os().skip(1);
    let path = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("fixture.pst"));
    let stress = args.any(|arg| arg == "--stress");
    let body = if stress {
        "Large Unicode body 본문. ".repeat(1_000)
    } else {
        "Hello from Memex PST export.".to_string()
    };
    let message = UnicodePstMessage {
        subject: "Memex PST export",
        sender_name: "Memex Sender",
        sender_email: "sender@example.com",
        recipient_name: "Memex Recipient",
        recipient_email: "recipient@example.com",
        body: &body,
        message_id: "<memex-pst-export@example.com>",
        delivery_time: 133_750_080_000_000_000,
    };
    if stress {
        let data = vec![0xA5; 320 * 1_024];
        let attachment = UnicodePstAttachment {
            filename: "large.bin",
            mime_type: "application/octet-stream",
            data: &data,
        };
        drop(UnicodePstFile::create_with_attachments(
            &path,
            &message,
            &[attachment],
        )?);
        for index in 1..10 {
            let subject = format!("Appended message {index}");
            let message_id = format!("<append-{index}@example.com>");
            drop(UnicodePstFile::append(
                &path,
                &UnicodePstMessage {
                    subject: &subject,
                    message_id: &message_id,
                    body: "Incrementally appended body.",
                    ..message
                },
            )?);
        }
    } else {
        drop(UnicodePstFile::create(&path, &message)?);
    }
    println!("{}", path.display());
    Ok(())
}
