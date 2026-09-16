# LIST corpus (scrubbed)

Raw `LIST` replies from the owner's three printers, taken read-only on
2026-09-15 (`LIST <dir>` over implicit FTPS :990, banner `220 BBL-P003 FTP
Server`, firmware A1 01.08.01.00 and P1S 01.10.00.00). They are the corpus
the design doc counts as 4730 lines (design doc 5.2 and Appendix A.2), and
`src/ftp.rs` parses every one of them in its tests.

| File | Printer | Entries | Unreadable (`?`) names |
|---|---|---|---|
| `a1.txt` | A1 (damaged SD card, cross-linked `/recorder`) | 3047 | 2080 |
| `a1_combo.txt` | A1 Combo | 1076 | 0 |
| `p1s.txt` | P1S | 607 | 0 |

## Format

One file per printer. A line starting with `# dir ` opens a directory and
every line after it is a verbatim LIST line of that directory, in the order
the server sent them (a LIST line always starts with `-`, `d` or `l`, so the
two never collide). CRLF was stripped, as suppaftp strips it. Nothing else
in the lines was reformatted: permissions, link count, owner, group, size,
date and name are exactly as the printers wrote them, including the `?`
names of the damaged card, the CJK and en-dash names, and the duplicated
listings that A1 #1's cross-linked `/recorder` produces.

## How they were scrubbed

Printer serial numbers appear in the file names under `/logger` and
`/recorder` (design doc 3.2), so those names are replaced before anything is
committed:

1. Every name under `/logger` and `/recorder`, at any depth, is replaced by
   `file_NNNN`, keeping the extension when the name had one (`file_0002.log`,
   `file_0731.bin`), numbered in the order the names appear. Nothing of the
   original name survives: not the serial, not the timestamp, not the
   firmware version, so the shape of a printer's log names cannot be read
   back from here either. Generic directory names (`cache`, `image`,
   `ipcam`, `model`, `recorder`, `timelapse`, `hms`, `md5`, `latest`,
   `System Volume Information`) and the unreadable `?` names are kept,
   because they carry nothing.
2. Any serial-shaped token (15 characters, `[0-9]{2}[0-9A-Z]{13}`) left
   anywhere else would be replaced too; step 1 leaves none.
3. The permissions, link count, owner, group, size and date of those lines
   are untouched, so 961 names changed and no other byte did.

Checked on the committed files: no configured serial, IP address or access
code appears; no serial-shaped token appears at all, not even the synthetic
`01P00Z9X8W7V6U5` the other fixtures use; and the longest run of consecutive
characters of any real serial found anywhere is 4 characters, all of them
inside dates and md5-style file names, where they are coincidences of hex
digits.

Nothing here is regenerated automatically: a new capture is scrubbed the same
way before it is committed, and the check above is re-run.
