use outlook_pst::{
    UnicodePstAttachment, UnicodePstFile, UnicodePstMessage, UnicodePstRecipient,
    UnicodePstRecipientType,
};
use std::{env, io, path::PathBuf};

const INLINE_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5, 0x1c, 0x0c,
    0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0xf8, 0x0f, 0x00,
    0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

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
    let html = if stress {
        format!(
            "{}<img src=\"cid:memex-inline@example.com\">",
            "<p>Large HTML body 본문.</p>".repeat(1_000)
        )
    } else {
        "<p>Hello from <strong>Memex PST export</strong>.</p><img src=\"cid:memex-inline@example.com\">"
            .to_string()
    };
    let recipients = [
        UnicodePstRecipient {
            name: "Memex Recipient",
            email: "recipient@example.com",
            recipient_type: UnicodePstRecipientType::To,
        },
        UnicodePstRecipient {
            name: "Memex CC",
            email: "cc@example.com",
            recipient_type: UnicodePstRecipientType::Cc,
        },
        UnicodePstRecipient {
            name: "Memex BCC",
            email: "bcc@example.com",
            recipient_type: UnicodePstRecipientType::Bcc,
        },
    ];
    let message = UnicodePstMessage {
        subject: "Memex PST export",
        sender_name: "Memex Sender",
        sender_email: "sender@example.com",
        recipients: &recipients,
        body: &body,
        html_body: Some(&html),
        message_id: "<memex-pst-export@example.com>",
        delivery_time: 133_750_080_000_000_000,
    };
    let inline_attachment = UnicodePstAttachment {
        filename: "inline.png",
        mime_type: "image/png",
        content_id: Some("<memex-inline@example.com>"),
        data: INLINE_PNG,
    };
    if stress {
        let data = vec![0xA5; 320 * 1_024];
        let attachment = UnicodePstAttachment {
            filename: "large.bin",
            mime_type: "application/octet-stream",
            content_id: None,
            data: &data,
        };
        let empty_attachment = UnicodePstAttachment {
            filename: "empty.txt",
            mime_type: "text/plain",
            content_id: None,
            data: &[],
        };
        drop(UnicodePstFile::create_with_attachments(
            &path,
            &message,
            &[inline_attachment, attachment, empty_attachment],
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
        drop(UnicodePstFile::create_with_attachments(
            &path,
            &message,
            &[inline_attachment],
        )?);
    }
    drop(UnicodePstFile::append_in_folder(
        &path,
        &["Imported", "2026"],
        &UnicodePstMessage {
            subject: "Message in Imported/2026",
            message_id: "<memex-folder-message@example.com>",
            ..message
        },
    )?);
    println!("{}", path.display());
    Ok(())
}
