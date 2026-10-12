# Third-party notices

Acre's original source code is available under MIT or Apache-2.0. Dependencies
and embedded assets retain their own licenses.

| Component | Source | License |
| --- | --- | --- |
| GPUI Kit 0.7.0 and GPUI Component | [longbridge/gpui-kit](https://github.com/longbridge/gpui-kit) | [Apache-2.0](third_party/gpui-kit-LICENSE-APACHE) |
| GPUI (`gpui-pre` 0.3.7) | [zed-industries/zed](https://github.com/zed-industries/zed) | [Apache-2.0](third_party/gpui-LICENSE-APACHE) |
| Lucide icons bundled with GPUI Kit | [lucide-icons/lucide](https://github.com/lucide-icons/lucide) | [ISC, with MIT for Feather-derived icons](third_party/lucide-LICENSE) |
| Inter font | [rsms/inter](https://github.com/rsms/inter) | [SIL Open Font License 1.1](third_party/inter-OFL.txt) |
| Noto Sans SC font | [notofonts/noto-cjk](https://github.com/notofonts/noto-cjk) | [SIL Open Font License 1.1](third_party/notosanssc-OFL.txt) |
| NTFS scanner code adapted from mathom, via Acre Core | [gitRasheed/mathom](https://github.com/gitRasheed/mathom) | [MIT](third_party/mathom-LICENSE) |

GPUI Kit is copyright 2024–2026 Longbridge. GPUI is copyright 2022–2025 Zed
Industries, Inc. Lucide icons are copyright 2026 Lucide Icons and Contributors;
Feather-derived icons are copyright 2013–present Cole Bemis. Inter is copyright
2016 The Inter Project Authors. The bundled Noto font license credits copyright
2014–2021 Adobe, with the reserved font name "Source". The adapted mathom code
is copyright 2026 Rasheed Khoshnaw. The full notices above accompany release
archives.

The GPUI packages pinned by this project declare Apache-2.0; the licensing of
the complete Zed editor is separate from these library packages.

`Cargo.lock` records the exact Rust dependency versions. Their package metadata
and distributed license files govern each dependency. The table above identifies
the GUI foundation, embedded fonts, and code adapted by the scanner; it is not
an inventory of every transitive dependency.
