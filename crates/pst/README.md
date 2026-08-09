# outlook-pst-rw

This is an MIT-licensed fork of Microsoft's `outlook-pst` crate. It preserves the reader API and adds narrowly scoped, append-only creation of new Unicode PST files. It does not modify existing PST files.

The PST file format is publicly documented in the [MS-PST](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/141923d5-15ab-4ef1-a524-6dce75aae546) open specification. Data structures and type names generally mimic the concepts and names in that document, with some adjustment for readability and to match Rust language conventions. As much as possible, everything in this crate should have a deep link to the documentation it is based on in the doc comments. 

## New-file writer

The format-spike API creates a message store, IPM subtree, Inbox, and one plain-text message with one recipient. Generate a compatibility fixture with:

```sh
cargo run -p outlook-pst-rw --example gen-fixture-pst -- fixture.pst
```

Existing PST mutation remains unsupported. Follow the [Maintaining Data Integrity](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/5e1a4d6b-ebbf-4658-9aa7-824929233044) guidance when extending the writer.
