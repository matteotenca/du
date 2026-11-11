# du for Windows

A native Windows port of GNU coreutils `du`, written in Rust with no dependencies beyond the standard library. Runs from `cmd.exe`, PowerShell or Git Bash.

## Build

```
rustc --edition 2024 -O src/main.rs -o target\release\du.exe
```

or, with Cargo:

```
cargo build --release
```

The result is `target\release\du.exe`. Build from PowerShell or `cmd.exe`: Git Bash puts its own `link` first in `PATH` and breaks linking. Run the unit tests with `cargo test`, or:

```
rustc --edition 2024 --test src/main.rs -o target\test_du.exe && target\test_du.exe
```

## Usage

```
du [OPTION]... [FILE]...
du [OPTION]... --files0-from=F
```

With no `FILE`, `du` reports the current directory. Paths can use `\` or `/`, drive letters (`f:\foo\bar`, `F:/foo/bar`), UNC names (`\\server\share`) and paths longer than 260 characters. The output keeps the separator style you typed.

Wildcards (`*` and `?`) in the last component of an operand are expanded, because `cmd.exe` and PowerShell do not expand them for native programs.

### Examples

```
du                          # disk usage of each subdirectory of the current directory, in 1K blocks
du -sh f:\foo\bar           # one human-readable total
du -ah -d1 .                # files and directories, one level deep
du -Ab src                  # apparent size in bytes
du -c -BM *.log             # grand total in megabytes
du --exclude=*.o --exclude=target -sh .
du -h -t 10M .              # only entries of at least 10 MiB
du --time -s .              # add the latest modification time
du --files0-from=list.bin   # NUL-separated names from a file ('-' for stdin)
```

### Options

| Option | Meaning |
| --- | --- |
| `-0`, `--null` | end each output line with NUL, not newline |
| `-a`, `--all` | write counts for all files, not just directories |
| `-A`, `--apparent-size` | print apparent sizes rather than disk usage |
| `-B`, `--block-size=SIZE` | scale sizes by `SIZE` (for example `-BM`); a bare unit (`-BM`, `-BKiB`, `-BB`) prints the unit after each number, `-B2M` does not |
| `-b`, `--bytes` | same as `--apparent-size --block-size=1` |
| `-c`, `--total` | produce a grand total |
| `-D`, `-H`, `--dereference-args` | follow only links given on the command line |
| `-d`, `--max-depth=N` | print totals only for entries `N` or fewer levels deep; `-d0` is `-s` |
| `--files0-from=F` | read NUL-terminated names from file `F` (`-` is stdin) |
| `-h`, `--human-readable` | sizes like `1K 234M 2G` (powers of 1024) |
| `--si` | like `-h`, with powers of 1000 |
| `--inodes` | list entry counts instead of sizes |
| `-k` | like `--block-size=1K` |
| `-m` | like `--block-size=1M` |
| `-L`, `--dereference` | follow all links |
| `-P`, `--no-dereference` | do not follow links (default) |
| `-l`, `--count-links` | count hard-linked files every time |
| `-S`, `--separate-dirs` | leave subdirectory sizes out of the parent total |
| `-s`, `--summarize` | display only a total for each argument |
| `--show-errors` | print error messages to stderr (default: silent) |
| `--stop-on-error` | stop at the first unreadable file or directory (default: keep going) |
| `-t`, `--threshold=SIZE` | skip entries smaller than `SIZE` (larger than `-SIZE` if negative) |
| `--time[=WORD]` | show the latest time in each tree: `mtime` (default), `atime`/`access`/`use`, `ctime`/`status`, `birth`/`creation` |
| `--time-style=STYLE` | `full-iso`, `long-iso`, `iso` or `+FORMAT` (strftime-like) |
| `-X`, `--exclude-from=FILE` | exclude files matching the patterns in `FILE` |
| `--exclude=PATTERN` | exclude files matching `PATTERN` |
| `-x`, `--one-file-system` | skip directories on other volumes |
| `--help`, `--version` | show help or version and exit |

Long options can be abbreviated to any unambiguous prefix, and options can follow operands.

`SIZE` is an integer with an optional unit: `B` (bytes), `K M G T P E Z Y` (powers of 1024), `KB MB ...` (powers of 1000) or `KiB MiB ...` (powers of 1024, same as `K M ...`). Without a number the unit is printed after each size (`-BM` gives `12M`, `-BKB` gives `12kB`, `-BB` gives `12345B`); with a number it is not (`-B2M` gives `6`).

### Environment

- `DU_BLOCK_SIZE`, `BLOCK_SIZE`, `BLOCKSIZE`: default block size (first one set wins).
- `POSIXLY_CORRECT`: default block size becomes 512 instead of 1024.
- `TIME_STYLE`: default for `--time-style`.

### Exit status

`0` even when some files or directories could not be read: by default errors are not printed and the scan continues. With `--stop-on-error` the scan stops at the first such error and the status is `1`. An invalid option or argument always gives `1`.

## Windows behavior

- **Disk usage** is the NTFS allocation size of each file, rounded up to 512 bytes like `st_blocks`. Compressed and sparse files report what they really occupy.
- **Directories** count as 0 bytes themselves; only their contents add up.
- **Hard links** are counted once, unless `-l` is given.
- **Symlinks and junctions** are not followed by default, so a junction given as an argument reports just the link. Use `-D` (command-line arguments only) or `-L` (everything) to follow them. Link cycles are detected and reported.
- **Exclude patterns** match case-insensitively, against the whole path or any trailing run of path components. `\` in a pattern is a path separator, not an escape.
- **Times** use the Windows local time zone; `TZ` is ignored. `%Z` in `--time-style` prints the UTC offset, not the zone name. `ctime` is the NTFS change time and `birth` is the creation time.

## Performance

Directories are read in bulk with `NtQueryDirectoryFile`, which returns size, allocation, times, file id and reparse tag for hundreds of entries per call, so regular files are never opened. Subdirectories are opened relative to their parent (no path-length limit) and scanned by one worker thread per core. The tree is then replayed in order, so output, hard-link handling and error messages are identical to a single-threaded run.

Hard links are detected by file id (two entries with the same volume and id are the same file).

Because sizes and times come from the directory entries, a file that is being written to right now can report slightly stale values, as with `dir`.

## Differences from GNU du

- Paths are printed as given, without terminal quoting, and `QUOTING_STYLE` is ignored.
- Unreadable files and directories are skipped silently and the exit status stays `0`. GNU `du` prints a message and exits `1`. Use `--show-errors` to print the messages and `--stop-on-error` to stop at the first error with status `1`.
- Sizes are not clamped to `Infinity` on overflow.
- Volumes are identified by a 32-bit serial and a 64-bit file index, so ReFS 128-bit file IDs are not used.
