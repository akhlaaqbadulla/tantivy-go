> ## This is a fork
>
> `akhlaaqbadulla/tantivy-go`, forked from `anyproto/tantivy-go` at `v1.0.6`.
>
> **What it adds:** two query types, `FuzzyTermQuery` and `OneOfFuzzyTermQuery`,
> for typo-tolerant matching.
>
> They do **not** wrap `tantivy::query::FuzzyTermQuery`. That type's `weight()`
> ignores the `EnableScoring` it is handed and always returns an
> `AutomatonWeight`, whose scorer is a `ConstScorer` — every match scores
> identically, so dropping one into a ranked boolean query erases BM25 for that
> clause. Instead the Levenshtein automaton is used only to **enumerate**: each
> analyzed token is streamed against every segment's term dictionary, and the
> matching terms become ordinary `TermQuery`s, each boosted down by its edit
> distance. Scoring survives, and a near-miss can rank without winning.
>
> The edit budget is narrowed per token, on the *analyzed* form: no token
> containing a digit is ever fuzzed, and length gates one edit at 5 characters
> and two at 9. Expansion is capped at 50 terms per token.
>
> **What it changes otherwise:** the Go module path, so a consumer can point a
> `replace` directive at it; `rust/Cargo.lock` is committed, because the crate
> is built into a pinned artifact; and `.github/workflows/themis-release.yml`
> builds `linux-amd64-musl` only. Every other target still comes from upstream
> and does **not** carry the fuzzy variants.
>
> Nothing else is modified. The wire format is append-only: the two new query
> types are ordinals 8 and 9, so an older library reading a newer payload
> rejects it loudly rather than misinterpreting an existing type.

# Go Tantivy Bindings

This project provides Go bindings for the [Tantivy](https://github.com/quickwit-oss/tantivy) search engine library. Tantivy is a full-text search engine library written in Rust, and this project aims to make its powerful search capabilities available to Go developers.

The library is thread safe and can be used in a concurrent environment

# Why

The only available FTS engine in the Golang community is [Bleve](https://github.com/blevesearch/bleve), which is surprisingly slow compared to [Tantivy](https://github.com/quickwit-oss/tantivy).
Check out the last link for details on the performance comparison.

![Search Benchmark](https://github.com/quickwit-oss/tantivy/blob/main/doc/assets/images/searchbenchmark.png)
Credits for the image to the Tantivy team

# Our Journey with Tantivy
We've been running it in [Anytype](https://github.com/anyproto/anytype-heart) for over a year across all major platforms and architectures without issues on 32-bit and 64-bit systems, x86 and ARM64, iOS, Android, PC, macOS, and Linux.

## Features
### Jieba Tokenizer
This library includes the Jieba feature by default, which provides Chinese text segmentation. However, if you do not need this functionality, you can build the library without it to save approximately 5MB of the dictionary.
### Golang API to Create Custom Queries for Tantivy
See `searchquerybuilder.go`

## Search quality testing
[Test quality](testquality/README.md)

## Installation

```bash
go get github.com/anyproto/tantivy-go
```

Ensure your libraries are in your `ld` path.

### Example Run
- Run `make download-tantivy-all` inside the `rust` folder
- Run `main.go` in the `example` folder

## Development
Development and compilation are done on MacBooks and for Apple platforms. Therefore, the development steps provided are for macOS.

### Install environment
- [Install rustup](https://rust-lang.github.io/rustup/installation/other.html)
- Install Rust architectures: `make setup`
- Add Android libraries to your path: `export PATH=$PATH:$ANDROID_HOME/tools:$ANDROID_HOME/emulator:$ANDROID_HOME/platform-tools:$ANDROID_HOME/ndk/25.2.9519653/toolchains/llvm/prebuilt/darwin-x86_64/bin`
- Install Windows compiler:  `brew install mingw-w64`
- Install musl: `brew tap messense/macos-cross-toolchains && brew install x86_64-unknown-linux-musl`

### Install rust libraries
Run inside the `rust` folder:

`make install-all` - install release versions for all platforms

`make install-debug-all` - install debug versions for all platforms

`make install-ARCH-GOOS` - install release version for ARCH GOOS

`make install-debug-ARCH-GOOS` - install debug version for ARCH GOOS

### GCC support
To be done

### Validate min macos version

`otool -l libtantivy_go.a  | rg LC_BUILD_VERSION -A4 | rg minos | sort | uniq -c`
Expected output:
```
 880     minos 11.0
```

### Possible troubleshooting
If you experience SIGSEGV issues with musl or windows, try adding these flags to the linker:
```
-extldflags '-static -Wl,-z stack-size=1000000'
```

### Nix

`flake.nix` currently provides two versions of `devShell`: musl and gcc.

This command will make a bash shell with all required build dependencies:

```bash
nix develop .
```

Each `devShell` also contains a script which:

- builds rust into `.a` lib
- copies it to `../anytype-heart`
- builds `anytype-heart` `grpcServer`
- copies `grpcServer` to `../anytype-ts` `anytypeHelper`

> [!TIP]
> To enable musl, set `musl = true;` in `flake.nix`.

If you want to debug `tantivy` from `anytype-ts`, with `musl` or `gcc`, this scripts automates all the flow.

All together it would look like:
```bash
nix develop .
tantivy_compile_all_gcc
# or
tantivy_compile_all_musl
```

To check that it works, run `anytype-ts` and try to search something.

> [!NOTE]
> MacOS (Darwin) nix shell is not supported yet
