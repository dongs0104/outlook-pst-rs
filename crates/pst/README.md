# outlook-pst-rw

This is an MIT-licensed fork of Microsoft's `outlook-pst` crate. It preserves the reader API and adds narrowly scoped creation and incremental append support for Unicode PST files.

The PST file format is publicly documented in the [MS-PST](https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-pst/141923d5-15ab-4ef1-a524-6dce75aae546) open specification. Data structures and type names generally mimic the concepts and names in that document, with some adjustment for readability and to match Rust language conventions. As much as possible, everything in this crate should have a deep link to the documentation it is based on in the doc comments. 

## Writer

The writer API creates a message store, IPM subtree, Inbox, and messages with TO, CC, and BCC recipients. It stores optional UTF-8 HTML in `PR_HTML` while retaining `PR_BODY` as the plain-text fallback. Generate a compatibility fixture with:

```sh
cargo run -p outlook-pst-rw --example gen-fixture-pst -- fixture.pst
cargo run -p outlook-pst-rw --example gen-fixture-pst -- stress.pst --stress
```

`UnicodePstFile::append` adds messages to the default receive folder of an existing unencrypted Unicode PST without replacing existing messages. It preserves the folder property context, contents-table schema and rows, and existing subnodes. `create_with_attachments` and `append_with_attachments` write by-value attachments, including zero-byte files; large bodies, HTML, and attachment data use XBLOCK/XXBLOCK data trees. Multi-level BBT, NBT, and subnode trees and multiple AMap regions are supported.

ANSI PST append and encoded/encrypted PSTs remain unsupported. Copy-on-write append currently retains superseded blocks, so repeated appends grow the file until a future compaction pass is implemented.
