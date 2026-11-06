# Katharistís Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Aplikasi Windows berbahasa Rust dengan peta disk, pembersih, uninstall aplikasi, pencari duplikat, dan CLI hanya-baca, yang keputusan menghapusnya selalu di tangan pengguna.

**Architecture:** Workspace Cargo dengan tiga crate. `katharistis-core` memuat semua logika dan panggilan Win32 tanpa bergantung pada GUI. `katharistis-gui` (egui/eframe) dan `katharistis-cli` hanya memanggil core. Semua penghapusan melewati satu pintu, `core::delete::delete`, yang selalu menjalankan `core::protect`.

**Tech Stack:** Rust stable (1.99, target `x86_64-pc-windows-msvc`), `windows` (Win32), `rayon`, `eframe`/`egui`, `clap`, `sha2`, `winreg`, `serde` + `toml`, `tempfile` (dev).

**Spec:** `docs/superpowers/specs/2026-11-06-katharistis-design.md`

## Global Constraints

- Windows 10 ke atas. Tanpa dukungan macOS atau Linux.
- Berjalan tanpa hak admin (`asInvoker`). Tanpa installer. Dua binary: `katharistis-gui.exe` dan `katharistis.exe`.
- Tanpa jaringan dan telemetri. Jaringan hanya lewat winget, dan hanya setelah pengguna menekan tombolnya.
- File pribadi dan sisa uninstall tidak pernah tercentang otomatis.
- Gagal pindah ke Recycle Bin tidak pernah otomatis menjadi hapus permanen.
- CLI tidak pernah menghapus.
- Nama tampilan `Katharistís`. Nama teknis `katharistis`. Pengaturan di `%APPDATA%\Katharistis\settings.toml`.
- Teks antarmuka tersedia dalam Bahasa Indonesia dan Inggris.
- Lisensi GPL-3.0 dengan pencantuman CleanBro sebagai proyek asal.
- Test tidak pernah menyentuh data asli pengguna. Semua test memakai folder `tempfile` dan `ProtectContext` buatan.

## Review Focus

1. File yang tidak bisa masuk Recycle Bin (terlalu besar, drive USB atau jaringan, Recycle Bin dimatikan): Windows diam-diam menghapus permanen kalau dibiarkan, padahal pengguna harus ditanya. Test di Task 6 memakai drive `subst`.
2. Path yang menyamarkan lokasi terlindungi (lewat junction, short name 8.3, titik atau spasi di akhir, awalan `\\?\`): harus tetap ditolak. Test di Task 2.
3. Folder OneDrive dengan file yang belum terunduh: harus terhitung tanpa memicu unduhan, dan tidak dibaca saat mencari duplikat. Test di Task 3 dan Task 8.
4. Uninstaller yang langsung keluar padahal masih berjalan, atau dibatalkan pengguna: sisa file tidak boleh ditawarkan selama aplikasi masih terdaftar. Test di Task 9.
5. File berubah atau hilang antara pemindaian dan penghapusan: dilaporkan apa adanya, tidak dihitung sebagai ruang yang didapat, dan tidak menghapus salinan terakhir duplikat. Test di Task 6 dan Task 8.

---

## Struktur file

```
katharistís/
├── Cargo.toml                 # workspace, profil release (lto, strip)
├── LICENSE  README.md  .gitignore
└── crates/
    ├── core/src/
    │   ├── lib.rs             # re-export modul
    │   ├── format.rs          # format_size
    │   ├── win.rs             # wrapper Win32: list_dir, long path, ukuran, file id, volume
    │   ├── protect.rs         # canonical path dan aturan terlindungi
    │   ├── scan.rs            # pohon hasil pindai
    │   ├── treemap.rs         # layout squarified
    │   ├── other_space.rs     # penjelasan ruang "Lainnya"
    │   ├── delete.rs          # satu pintu penghapusan
    │   ├── recycle.rs         # IFileOperation + sink, kosongkan Recycle Bin
    │   ├── junk.rs            # kategori Pembersihan
    │   ├── analysis.rs        # terbesar dan lama tak diubah
    │   ├── dupes.rs           # duplikat
    │   ├── apps/{mod,registry,uninstall,winget,leftovers,marks}.rs
    │   └── settings.rs
    ├── cli/src/{main,render}.rs
    └── gui/src/
        ├── main.rs  app.rs    # jendela, navigasi, tema
        ├── i18n.rs            # l(en, id)
        ├── jobs.rs            # pekerjaan latar
        ├── confirm.rs         # dialog konfirmasi dan laporan penghapusan
        └── views/{home,clean,apps,large,stale,dupes,settings}.rs
```

Acuan perilaku: source CleanBro di `https://github.com/jetorbitdev/cleanbro`. Kalau spec dan CleanBro berbeda, ikuti spec.

---

### Task 1: Workspace dan format ukuran

**Files:**
- Create: `Cargo.toml`, `.gitignore`, `LICENSE`, `README.md`, `crates/core/Cargo.toml`, `crates/core/src/{lib,format}.rs`, `crates/cli/Cargo.toml`, `crates/cli/src/main.rs`, `crates/gui/Cargo.toml`, `crates/gui/src/main.rs`

**Interfaces:**
- Produces: paket `katharistis-core` (lib `katharistis_core`), `katharistis-cli` (bin `katharistis`), `katharistis-gui` (bin `katharistis-gui`). `pub fn format_size(bytes: u64) -> String`.

- [ ] **Step 1: Jalankan `git init` di folder `katharistís`, buat workspace, dan salin teks GPL-3.0 ke `LICENSE`**

Profil release: `lto = true`, `strip = true`, `opt-level = "s"`. `README.md` menyebut asal dari CleanBro (`https://github.com/jetorbitdev/cleanbro`, Copyright (C) 2026 gilang), lisensi, dan peringatan SmartScreen. `.gitignore`: `target/`.

- [ ] **Step 2: Tulis test yang gagal di `format.rs`**

```rust
#[test]
fn formats_with_binary_units() {
    assert_eq!(format_size(0), "0 B");
    assert_eq!(format_size(1023), "1023 B");
    assert_eq!(format_size(1536), "1.5 KiB");
    assert_eq!(format_size(38 * 1024 * 1024 * 1024), "38.0 GiB");
}
```

- [ ] **Step 3: Pastikan gagal:** `cargo test -p katharistis-core format` gagal karena `format_size` belum ada.
- [ ] **Step 4: Implementasikan `format_size`.**
- [ ] **Step 5: Pastikan lolos:** `cargo test -p katharistis-core format` dan `cargo build --workspace` berhasil. Ini juga membuktikan linker MSVC bekerja.
- [ ] **Step 6: Commit:** `chore: scaffold workspace with size formatting`

---

### Task 2: Aturan path terlindungi

**Files:**
- Create: `crates/core/src/win.rs`, `crates/core/src/protect.rs`

**Interfaces:**
- Produces:
  - `win::to_long_path(p: &Path) -> Vec<u16>`: path null-terminated dengan awalan `\\?\` untuk path absolut.
  - `protect::canonical_key(p: &Path) -> Option<String>`: `None` untuk path relatif.
  - `protect::canonical_for_delete(p: &Path) -> Option<PathBuf>`: resolusi final folder induk lewat `GetFinalPathNameByHandleW`, komponen terakhir tidak diikuti. Kalau path sudah tidak ada, berlaku secara leksikal.
  - `pub struct ProtectContext { pub windows_dir, pub program_files: Vec<PathBuf>, pub program_data, pub users_dir, pub profile, pub own_dirs: Vec<PathBuf> }`, `ProtectContext::from_env() -> Self`
  - `protect::is_protected(p: &Path, ctx: &ProtectContext) -> bool`

`from_env` mengisi `own_dirs` dengan `%APPDATA%\Katharistis` dan path file `.exe` yang sedang berjalan, bukan folder tempat `.exe` berada. Kalau pengguna menjalankan aplikasi dari `Downloads`, isi `Downloads` tetap harus bisa dihapus.

Aturan `canonical_key`: buang `\\?\` dan `\\?\UNC\`, ubah `/` menjadi `\`, selesaikan `.` dan `..`, buang `\` di akhir, buang titik dan spasi di akhir tiap komponen seperti yang dilakukan Win32, tolak komponen yang memuat `:` selain huruf drive (alternate data stream), jalankan `GetLongPathNameW` kalau ada, lalu `to_lowercase`.

Aturan `is_protected` (urutan bebas, path relatif selalu `true`):
- Persis: akar drive, `windows_dir`, `program_data`, `users_dir`, `profile`, dan di bawah `profile`: `Documents`, `Desktop`, `Downloads`, `Pictures`, `Music`, `Videos`, `AppData`, `AppData\Local`, `AppData\Roaming`.
- Beserta isi: `windows_dir`, tiap `program_files`, `profile\.ssh`, `profile\.gnupg`, tiap `own_dirs`.
- Di drive mana pun, komponen pertama setelah akar: `System Volume Information`, `$Recycle.Bin` (beserta isi), `pagefile.sys`, `hiberfil.sys`, `swapfile.sys`.

- [ ] **Step 1: Tulis test yang gagal di `protect.rs`**

```rust
fn ctx() -> ProtectContext { /* C:\Windows, C:\Program Files, C:\Program Files (x86),
    C:\ProgramData, C:\Users, C:\Users\me, own: C:\Users\me\AppData\Roaming\Katharistis */ }

#[test] fn blocks_exact_and_subtree() {
    let c = ctx();
    for p in [r"C:\", r"D:\", r"C:\Windows", r"C:\Windows\System32\kernel32.dll",
              r"C:\Program Files\App\a.exe", r"C:\ProgramData", r"C:\Users", r"C:\Users\me",
              r"C:\Users\me\Documents", r"C:\Users\me\AppData\Local", r"C:\Users\me\.ssh\id_rsa",
              r"D:\System Volume Information\x", r"D:\$Recycle.Bin\S-1\a", r"D:\pagefile.sys",
              r"C:\Users\me\AppData\Roaming\Katharistis\settings.toml", "relative\\file"] {
        assert!(is_protected(Path::new(p), &c), "{p}");
    }
}
#[test] fn allows_contents_of_standard_folders() {
    let c = ctx();
    for p in [r"C:\Users\me\Documents\a.txt", r"C:\Users\me\AppData\Local\Temp\x",
              r"C:\ProgramData\Vendor", r"D:\Games\old"] {
        assert!(!is_protected(Path::new(p), &c), "{p}");
    }
}
#[test] fn sees_through_disguises() {
    let c = ctx();
    for p in [r"\\?\C:\Windows\x", r"c:/WINDOWS/x", r"C:\Windows.\x", r"C:\Users\me\Documents ",
              r"C:\Users\me\x\..\Documents", r"C:\Windows\x:stream"] {
        assert!(is_protected(Path::new(p), &c), "{p}");
    }
}
```

Test nyata, memakai `tempfile` dan `cmd /c mklink /J`:

```rust
#[test] fn junction_into_protected_is_resolved_but_junction_itself_is_not() {
    // protected = <tmp>\guarded (lewat own_dirs); <tmp>\link -> junction ke guarded
    assert!(is_protected(&canonical_for_delete(&tmp.join(r"link\file.txt")).unwrap(), &c));
    assert!(!is_protected(&canonical_for_delete(&tmp.join("link")).unwrap(), &c));
}
#[test] fn short_name_is_expanded() { /* ambil 8.3 lewat GetShortPathNameW; lewati kalau volume tidak punya 8.3 */ }
```

- [ ] **Step 2: Pastikan gagal:** `cargo test -p katharistis-core protect`.
- [ ] **Step 3: Implementasikan `win.rs` dan `protect.rs`.**
- [ ] **Step 4: Pastikan lolos:** perintah yang sama.
- [ ] **Step 5: Commit:** `feat(core): add protected path rules`

---

### Task 3: Pemindaian

**Files:**
- Create: `crates/core/src/scan.rs`
- Modify: `crates/core/src/win.rs`

**Interfaces:**
- Consumes: `win::to_long_path`.
- Produces:
```rust
// win.rs
pub struct DirEntry { pub name: String, pub attrs: u32, pub reparse_tag: u32, pub modified: Option<SystemTime> }
pub fn list_dir(p: &Path) -> io::Result<Vec<DirEntry>>; // FindFirstFileExW, FindExInfoBasic, LARGE_FETCH
pub fn allocated_size(p: &Path) -> io::Result<u64>;     // GetCompressedFileSizeW
pub fn link_info(p: &Path) -> io::Result<(u64, u64, u32)>; // (volume, file id, jumlah link), CreateFileW + FILE_FLAG_OPEN_REPARSE_POINT
pub fn volume_space(p: &Path) -> io::Result<(u64, u64)>;   // (total, free)

// scan.rs
pub enum EntryClass { Dir, File, Link }
pub fn classify(attrs: u32, tag: u32) -> EntryClass;
pub fn is_cloud_only(attrs: u32) -> bool;
pub type NodeId = usize;
pub struct Node { pub name: String, pub size: u64, pub modified: Option<SystemTime>, pub class: EntryClass,
                  pub cloud: bool, pub parent: Option<NodeId>, pub children: Vec<NodeId> }
pub struct ScanError { pub path: PathBuf, pub reason: String }
pub struct ScanTree { pub nodes: Vec<Node>, pub root: NodeId, pub root_path: PathBuf,
                      pub errors: Vec<ScanError>, pub files: u64, pub dirs: u64, pub cancelled: bool }
impl ScanTree {
    pub fn path_of(&self, id: NodeId) -> PathBuf;
    pub fn children_by_size(&self, id: NodeId) -> Vec<NodeId>;
    pub fn remove_subtree(&mut self, id: NodeId); // lepas dari induk, kurangi ukuran semua leluhur
}
pub struct ScanOptions { pub threads: usize, pub excluded: Vec<PathBuf> }
#[derive(Clone, Default)] pub struct ScanProgress(/* Arc */);
impl ScanProgress { pub fn cancel(&self); pub fn is_cancelled(&self) -> bool; pub fn files(&self) -> u64; pub fn bytes(&self) -> u64; }
pub fn scan(root: &Path, opts: &ScanOptions, progress: &ScanProgress) -> io::Result<ScanTree>;
```

Keputusan:
- `classify`: `Link` kalau ada `FILE_ATTRIBUTE_REPARSE_POINT` dan tag adalah name surrogate (`tag & 0x20000000 != 0`). Jadi junction, symlink, dan mount point tidak diikuti, sedangkan tag cloud tetap dipindai. Selain itu `Dir` atau `File` menurut atribut.
- `is_cloud_only`: `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` atau `FILE_ATTRIBUTE_RECALL_ON_OPEN` atau `FILE_ATTRIBUTE_OFFLINE`.
- `link_info` hanya dengan `FILE_READ_ATTRIBUTES`, supaya tidak memicu unduhan.
- Ukuran file: `allocated_size`. Link berukuran 0.
- Hard link: kalau jumlah link lebih dari 1, `(volume, id)` masuk `Mutex<HashSet>`. Hanya kemunculan pertama yang dihitung ukurannya.
- `modified` folder: maksimum dari `modified` folder itu sendiri dan seluruh isinya.
- Rayon `ThreadPool` dengan `threads` dan `stack_size` 16 MiB. Rekursif, tiap level menghasilkan subtree lalu digabung ke `nodes` di akhir.
- Karena Link tidak pernah diikuti, volume lain tidak pernah dimasuki. Jadi `-x` di CLI diterima, tetapi perilakunya sudah selalu begitu. README mencatatnya.
- `excluded` dibandingkan lewat `protect::canonical_key`.
- `root` tanpa izin baca atau tidak ada mengembalikan `Err`. Subfolder yang gagal masuk `errors`.

- [ ] **Step 1: Tulis test yang gagal**

```rust
#[test] fn classify_follows_cloud_but_not_links() {
    const R: u32 = 0x400; const D: u32 = 0x10;
    assert!(matches!(classify(D | R, 0xA0000003), EntryClass::Link)); // junction
    assert!(matches!(classify(R, 0xA000000C), EntryClass::Link));     // symlink
    assert!(matches!(classify(D | R, 0x9000701A), EntryClass::Dir));  // folder cloud
    assert!(matches!(classify(R, 0x9000001A), EntryClass::File));     // file cloud
}
#[test] fn cloud_only_flags() { assert!(is_cloud_only(0x400000)); assert!(!is_cloud_only(0x20)); }
#[test] fn sums_and_sorts() { /* a\big 1 MiB, a\small 1 KiB: size a >= 1 MiB, anak terurut besar ke kecil */ }
#[test] fn hard_link_counted_once() { /* file 1 MiB + std::fs::hard_link: total sama dengan satu salinan */ }
#[test] fn junction_not_followed() { /* junction ke folder 1 MiB: size node 0, class Link */ }
#[test] fn unreadable_folder_is_recorded_not_fatal() { /* icacls /deny; errors.len()==1, file lain terhitung; pulihkan izin */ }
#[test] fn folder_date_is_newest_descendant() { /* set_modified pada file dalam; tanggal folder atas sama dengan file itu */ }
#[test] fn long_path_over_260() { /* buat dengan \\?\; terhitung */ }
#[test] fn excluded_is_skipped() {}
#[test] fn cancel_returns_partial() { /* progress.cancel() sebelum scan: cancelled == true, tanpa panic */ }
#[test] fn remove_subtree_updates_ancestors() {}
```

- [ ] **Step 2: Pastikan gagal:** `cargo test -p katharistis-core scan`.
- [ ] **Step 3: Implementasikan `win.rs` tambahan dan `scan.rs`.**
- [ ] **Step 4: Pastikan lolos:** perintah yang sama.
- [ ] **Step 5: Commit:** `feat(core): add parallel disk scan`

---

### Task 4: CLI

**Files:**
- Create: `crates/cli/src/render.rs`
- Modify: `crates/cli/src/main.rs`

**Interfaces:**
- Consumes: `scan::{scan, ScanTree, ScanOptions, ScanProgress}`, `win::volume_space`, `format_size`.
- Produces: `render::render(tree: &ScanTree, opts: &RenderOpts, volume: Option<(u64, u64)>, elapsed: Duration) -> String`, dengan `RenderOpts { depth: usize, top: usize, files: usize }`. Opsi clap persis seperti tabel spec: `-d`=1, `-n`=20, `-f`=15, `-x`, `-j`=CPU×4, path default `%USERPROFILE%`.

Bentuk keluaran mengikuti versi Go. Baris judul, baris ukuran dengan bar 20 karakter `█░` dan persentase, baris `… N folder lainnya`, daftar file terbesar, baris volume `💾`, dan peringatan untuk `errors`.

- [ ] **Step 1: Tulis test yang gagal di `render.rs`**

```rust
#[test] fn renders_bar_percent_and_rest() {
    // pohon buatan: root 100 GiB, anak besar 69 GiB, n=1
    let s = render(&tree, &RenderOpts { depth: 1, top: 1, files: 0 }, Some((228, 25)), elapsed);
    assert!(s.contains("69.0%")); assert!(s.contains("██████████████░░░░░░"));
    assert!(s.contains("1 folder lainnya"));
}
#[test] fn top_zero_shows_all_and_files_zero_hides_files() {}
#[test] fn warns_about_denied_folders() {}
```

- [ ] **Step 2: Pastikan gagal:** `cargo test -p katharistis-cli`.
- [ ] **Step 3: Implementasikan `render.rs` dan `main.rs`. Sertakan pesan dan kode keluar 1 kalau path tidak ada.**
- [ ] **Step 4: Pastikan lolos:** `cargo test -p katharistis-cli`.
- [ ] **Step 5: Coba nyata:** `cargo run -p katharistis-cli -- -d 2 -n 5 %USERPROFILE%\Documents` menampilkan pohon dan baris volume, dan bandingkan totalnya secara kasar dengan properti folder di Explorer.
- [ ] **Step 6: Commit:** `feat(cli): add read-only disk usage command`

---

### Task 5: Treemap dan ruang "Lainnya"

**Files:**
- Create: `crates/core/src/treemap.rs`, `crates/core/src/other_space.rs`

**Interfaces:**
- Consumes: `ScanTree`, `win::{list_dir, allocated_size, volume_space}`.
- Produces:
```rust
#[derive(Clone, Copy, PartialEq, Debug)] pub struct Rect { pub x: f32, pub y: f32, pub w: f32, pub h: f32 }
pub fn squarify(sizes_desc: &[u64], area: Rect) -> Vec<Rect>;
pub struct MapItem { pub node: Option<NodeId>, pub size: u64 } // node None = "Lainnya"
pub fn map_items(tree: &ScanTree, parent: NodeId, min_fraction: f32, max_items: usize) -> Vec<MapItem>;

pub struct OtherSpace { pub used: u64, pub scanned: u64, pub unaccounted: u64,
                        pub system_files: Vec<(String, u64)>, pub unreadable: Vec<ScanError> }
pub fn other_space(tree: &ScanTree) -> io::Result<OtherSpace>;
```
`map_items` memakai `min_fraction` 0.005 dan `max_items` 40 dari GUI, dan menggabungkan sisanya ke satu item `node: None`. `system_files` memeriksa `pagefile.sys`, `hiberfil.sys`, `swapfile.sys`, `System Volume Information` di akar drive. `unaccounted = used.saturating_sub(scanned)`.

- [ ] **Step 1: Tulis test yang gagal**

```rust
#[test] fn squarify_fills_area_without_overlap() {
    let r = squarify(&[60, 30, 10], Rect { x: 0., y: 0., w: 100., h: 50. });
    let area: f32 = r.iter().map(|q| q.w * q.h).sum();
    assert!((area - 5000.).abs() < 1.);
    assert!((r[0].w * r[0].h - 3000.).abs() < 1.);
    // tidak ada dua kotak yang bertumpuk
}
#[test] fn squarify_handles_zero_and_empty() { assert!(squarify(&[], area).is_empty()); assert_eq!(squarify(&[0, 0], area).len(), 2); }
#[test] fn map_items_groups_small_into_other() {}
#[test] fn map_items_keeps_total_equal_to_parent() {}
#[test] fn other_space_never_underflows() {}
```

- [ ] **Step 2: Pastikan gagal:** `cargo test -p katharistis-core -- treemap other_space`.
- [ ] **Step 3: Implementasikan keduanya. `squarify` memakai algoritma Bruls-Huizing-van Wijk.**
- [ ] **Step 4: Pastikan lolos.**
- [ ] **Step 5: Commit:** `feat(core): add treemap layout and other-space breakdown`

---

### Task 6: Penghapusan

**Files:**
- Create: `crates/core/src/delete.rs`, `crates/core/src/recycle.rs`

**Interfaces:**
- Consumes: `protect::{canonical_for_delete, is_protected, ProtectContext}`, `scan`.
- Produces:
```rust
pub enum DeleteMode { RecycleBin, Permanent }
pub struct Target { pub path: PathBuf, pub size: u64 }
pub enum FailReason { Protected, NotFound, InUse, AccessDenied, NotRecyclable, Other(String) }
pub struct Failed { pub path: PathBuf, pub size: u64, pub reason: FailReason }
#[derive(Default)] pub struct DeleteReport {
    pub removed: Vec<PathBuf>, pub recycled: Vec<PathBuf>, pub failed: Vec<Failed>,
    pub bytes_freed: u64, pub bytes_in_bin: u64 }
pub fn delete(targets: &[Target], mode: DeleteMode, ctx: &ProtectContext) -> DeleteReport;

// recycle.rs
pub enum PreDelete { Proceed, Abort }
pub fn decide_pre_delete(flags: u32) -> PreDelete;   // Proceed hanya kalau TSF_DELETE_RECYCLE_IF_POSSIBLE aktif
pub fn recycle_one(p: &Path) -> Result<(), FailReason>;
pub fn bin_info() -> io::Result<(u64, u64)>;  // (byte, jumlah item), SHQueryRecycleBinW semua drive
pub fn empty_bin() -> io::Result<()>;         // SHEmptyRecycleBinW, tanpa suara dan progres, tetap konfirmasi di GUI
```

Keputusan penting:
- **Jangan pakai crate `trash` atau `SHFileOperation` dengan `FOF_ALLOWUNDO`**, karena keduanya bisa menghapus permanen tanpa memberi tahu kalau item tidak bisa masuk Recycle Bin. Pakai `IFileOperation` dengan `FOF_ALLOWUNDO | FOF_NO_UI | FOFX_RECYCLEONDELETE` dan sink `IFileOperationProgressSink`. Di `PreDeleteItem`, kalau flag `TSF_DELETE_RECYCLE_IF_POSSIBLE` tidak aktif, kembalikan `E_ABORT`. Hasilnya `FailReason::NotRecyclable`, dan file tetap utuh.
- COM diinisialisasi per panggilan di thread pekerja (`COINIT_APARTMENTTHREADED`).
- Permanen memakai penjelajah sendiri, bukan `remove_dir_all`, supaya file yang terkunci hanya melewati file itu dan sisanya tetap terhapus. Link dihapus sebagai tautan (`remove_dir` atau `remove_file`), tanpa masuk ke targetnya. Atribut read-only dibersihkan lalu dicoba lagi sekali. Hapus sebagian dilaporkan sebagai `Failed` dengan `InUse`, dan `bytes_freed` bertambah sebesar yang benar-benar hilang (ukuran awal dikurangi sisa).
- `bytes_in_bin` tidak pernah masuk `bytes_freed`.
- Path yang sudah hilang: `NotFound`, tanpa menambah `bytes_freed`.
- Kode error Windows dipetakan: 32 dan 33 ke `InUse`, 5 ke `AccessDenied`, 2 dan 3 ke `NotFound`.

- [ ] **Step 1: Tulis test yang gagal**

```rust
#[test] fn refuses_protected_and_touches_nothing() {
    // own_dirs = tmp\guarded berisi file
    let r = delete(&[t(&guarded_file)], DeleteMode::Permanent, &ctx);
    assert!(matches!(r.failed[0].reason, FailReason::Protected)); assert!(guarded_file.exists());
}
#[test] fn permanent_removes_tree_and_counts_bytes() {}
#[test] fn permanent_does_not_follow_junction() { /* target isi junction tetap ada, junction hilang */ }
#[test] fn locked_file_is_reported_and_siblings_are_removed() {
    // buka file dengan share_mode(0); saudara hilang, file terkunci ada, reason InUse, bytes_freed > 0
}
#[test] fn missing_target_counts_nothing() {
    let r = delete(&[Target { path: gone, size: 1000 }], DeleteMode::Permanent, &ctx);
    assert!(matches!(r.failed[0].reason, FailReason::NotFound)); assert_eq!(r.bytes_freed, 0);
}
#[test] fn read_only_file_is_removed() {}
#[test] fn pre_delete_aborts_without_recycle_flag() {
    assert!(matches!(decide_pre_delete(TSF_DELETE_RECYCLE_IF_POSSIBLE), PreDelete::Proceed));
    assert!(matches!(decide_pre_delete(0), PreDelete::Abort));
}
#[test] fn recycle_moves_file_and_counts_as_in_bin_not_freed() {
    let r = delete(&[t(&f)], DeleteMode::RecycleBin, &ctx);
    assert!(!f.exists()); assert_eq!(r.recycled.len(), 1); assert_eq!(r.bytes_freed, 0); assert!(r.bytes_in_bin > 0);
}
#[test] fn drive_without_bin_never_deletes_silently() {
    // subst drive kosong ke folder temp. Drive subst tidak punya Recycle Bin.
    let r = delete(&[t(&file_on_subst)], DeleteMode::RecycleBin, &ctx);
    assert!(matches!(r.failed[0].reason, FailReason::NotRecyclable));
    assert!(file_on_subst.exists());
    // selalu jalankan subst /D saat selesai, juga kalau test gagal
}
```

Test Recycle Bin menaruh file uji sungguhan ke Recycle Bin mesin ini. File-nya berasal dari `tempfile` dan bernama `katharistis-test-*`. Test tidak pernah memanggil `empty_bin`. Pemanggilan nyata `empty_bin` hanya lewat GUI di Task 13, dengan konfirmasi.

- [ ] **Step 2: Pastikan gagal:** `cargo test -p katharistis-core -- delete recycle`.
- [ ] **Step 3: Implementasikan `recycle.rs` dan `delete.rs`.**
- [ ] **Step 4: Pastikan lolos.** Kalau `drive_without_bin_never_deletes_silently` gagal dan file hilang, berhenti dan laporkan. Itu kegagalan terpenting di seluruh proyek.
- [ ] **Step 5: Commit:** `feat(core): add single delete path with safe recycle`

---

### Task 7: Kategori Pembersihan

**Files:**
- Create: `crates/core/src/junk.rs`

**Interfaces:**
- Consumes: `scan`, `win`, `recycle::bin_info`.
- Produces:
```rust
pub enum JunkKind { Temp, BrowserCache, CrashLogs, DevData, GeneralCache, PartialDownloads, RecycleBin }
pub struct JunkItem { pub path: PathBuf, pub name: String, pub size: u64, pub checked: bool }
pub struct JunkCategory { pub kind: JunkKind, pub items: Vec<JunkItem>, pub unreadable: bool, pub bin_size: u64 }
pub struct JunkRoots { pub temp, pub local_appdata, pub profile, pub downloads: PathBuf }
impl JunkRoots { pub fn from_env() -> Self }
pub fn scan_junk(roots: &JunkRoots, now: SystemTime, progress: &ScanProgress) -> Vec<JunkCategory>;
```

Lokasi:
- `Temp`: anak `temp`. Item yang berubah dalam 24 jam terakhir tetap didaftar, tetapi `checked = false`, karena mungkin sedang dipakai installer. Keputusan pembuat.
- `BrowserCache`: `local_appdata\Google\Chrome\User Data\<profil>\{Cache,Code Cache}`, `...\Microsoft\Edge\User Data\<profil>\{Cache,Code Cache}`, `...\Mozilla\Firefox\Profiles\<profil>\cache2`.
- `CrashLogs`: anak `local_appdata\CrashDumps`, `local_appdata\Microsoft\Windows\WER`.
- `DevData`: `local_appdata\npm-cache\_cacache`, `local_appdata\pip\Cache`, `profile\.gradle\caches`, `local_appdata\NuGet\v3-cache`.
- `GeneralCache`: anak `profile\.cache`, `checked = false`.
- `PartialDownloads`: file di `downloads` berakhiran `.crdownload`, `.part`, `.partial`.
- `RecycleBin`: tanpa item, `bin_size` dari `bin_info()`, tidak tercentang.

Hanya `GeneralCache` dan `RecycleBin` yang tidak tercentang sebagai kategori. Link tidak pernah menjadi item.

- [ ] **Step 1: Tulis test yang gagal, memakai `JunkRoots` buatan di folder temp**

```rust
#[test] fn browser_cache_only_never_profile_data() {
    // User Data\Default\{Cache\x, Code Cache\y, Login Data, Bookmarks}
    // item hanya Cache dan Code Cache, tidak ada Login Data atau Bookmarks
}
#[test] fn general_cache_unchecked_others_checked() {}
#[test] fn recent_temp_items_unchecked() {}
#[test] fn downloads_only_partial_extensions() { /* a.part ya, b.zip tidak */ }
#[test] fn missing_folders_are_empty_not_errors() {}
#[test] fn link_inside_temp_is_ignored() {}
```

- [ ] **Step 2: Pastikan gagal:** `cargo test -p katharistis-core junk`.
- [ ] **Step 3: Implementasikan `junk.rs`.**
- [ ] **Step 4: Pastikan lolos.**
- [ ] **Step 5: Commit:** `feat(core): add cleanup categories`

---

### Task 8: Analisis dan duplikat

**Files:**
- Create: `crates/core/src/analysis.rs`, `crates/core/src/dupes.rs`

**Interfaces:**
- Consumes: `ScanTree`, `NodeId`, `ScanProgress`, `win::link_info`.
- Produces:
```rust
pub const LARGE_MIN: u64 = 100 * 1024 * 1024;
pub fn large_files(tree: &ScanTree, min_bytes: u64) -> Vec<NodeId>;   // terurut besar ke kecil
pub fn stale_files(tree: &ScanTree, older_than: Duration, now: SystemTime) -> Vec<NodeId>; // pilihan GUI: 6 bulan, 1 tahun, 2 tahun
pub struct DuplicateGroup { pub size: u64, pub keep: PathBuf, pub others: Vec<PathBuf> }
pub fn find_duplicates(tree: &ScanTree, min_bytes: u64, progress: &ScanProgress) -> Vec<DuplicateGroup>;
pub fn targets_still_safe(group: &DuplicateGroup, selected: &[PathBuf]) -> bool;
```
- Kelompokkan per ukuran, buang satu anggota tunggal, hash SHA-256 secara streaming.
- Lewati node dengan `cloud = true`. Hard link dengan `(volume, id)` yang sama dianggap satu file.
- `keep` adalah path terpendek, lalu urutan abjad, supaya stabil.
- `min_bytes` default GUI 1 MiB. Keputusan pembuat, untuk menghindari ribuan file kecil.
- `targets_still_safe` membaca ulang: `keep` masih ada, ukurannya sama, hash-nya masih cocok, dan `keep` tidak termasuk `selected`.

- [ ] **Step 1: Tulis test yang gagal**

```rust
#[test] fn large_and_stale_filter_and_sort() {}
#[test] fn duplicates_group_by_content_not_name() {}
#[test] fn same_size_different_content_not_grouped() {}
#[test] fn hard_links_are_not_duplicates() {}
#[test] fn cloud_files_are_never_read() { /* node cloud=true dengan path yang tidak ada: tanpa error, tidak masuk grup */ }
#[test] fn keep_is_never_in_others() {}
#[test] fn stops_being_safe_when_keeper_changes() {
    // ubah isi keep sesudah grup dibuat: targets_still_safe == false
}
#[test] fn cancel_stops_hashing() {}
```

- [ ] **Step 2: Pastikan gagal:** `cargo test -p katharistis-core -- analysis dupes`.
- [ ] **Step 3: Implementasikan keduanya.**
- [ ] **Step 4: Pastikan lolos.**
- [ ] **Step 5: Commit:** `feat(core): add large, stale, and duplicate analysis`

---

### Task 9: Daftar aplikasi dan uninstall

**Files:**
- Create: `crates/core/src/apps/{mod,registry,uninstall}.rs`

**Interfaces:**
- Produces:
```rust
pub struct AppEntry { pub key: String, pub name: String, pub publisher: Option<String>, pub version: Option<String>,
    pub install_date: Option<NaiveDateLike>, pub size: u64, pub install_location: Option<PathBuf>,
    pub uninstall: Option<String> }
pub fn entry_from_values(key: &str, get: &dyn Fn(&str) -> Option<RegValue>) -> Option<AppEntry>;
pub fn list_installed() -> Vec<AppEntry>;
pub fn still_registered(key: &str) -> bool;

pub struct UninstallCmd { pub file: String, pub args: String }
pub fn parse_uninstall(s: &str, exists: &dyn Fn(&str) -> bool) -> Option<UninstallCmd>;
pub fn run_uninstaller(cmd: &UninstallCmd) -> io::Result<UninstallEnd>;
pub enum UninstallEnd { Exited, Cancelled }
pub enum LeftoverState { Ready, StillInstalled }
pub fn leftover_state(key: &str, registered: &dyn Fn(&str) -> bool) -> LeftoverState;
```

Keputusan:
- `NaiveDateLike` adalah `(u16, u8, u8)`, diurai dari `InstallDate` `YYYYMMDD`. Nilai salah menjadi `None`.
- Sumber: `HKLM` dan `HKCU` `SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall`, plus `HKLM` `SOFTWARE\WOW6432Node\...`.
- `entry_from_values` membuang entri tanpa `DisplayName`, `SystemComponent=1`, dan yang punya `ParentKeyName`. Ukuran: `EstimatedSize` (KiB) × 1024. Entri yang sama lintas hive digabung berdasarkan `(nama, versi)`.
- `QuietUninstallString` tidak dipakai.
- `parse_uninstall` menangani path berkutip, `MsiExec.exe /X{GUID}`, dan path tanpa kutip yang memuat spasi, dengan memanjangkan token sampai `exists` benar.
- `run_uninstaller` memakai `ShellExecuteExW` supaya UAC muncul, lalu menunggu proses. Kode `ERROR_CANCELLED` menjadi `Cancelled`. Prosesnya tidak dipakai untuk menyimpulkan selesai, karena banyak uninstaller menyalin diri lalu langsung keluar. Penentunya `leftover_state`.

- [ ] **Step 1: Tulis test yang gagal**

```rust
#[test] fn filters_system_and_nameless_entries() {}
#[test] fn converts_size_and_date() { /* EstimatedSize 2048 → 2_097_152; "20240131" → (2024,1,31); "abc" → None */ }
#[test] fn parses_quoted_msi_and_unquoted_paths() {
    let e = |p: &str| p == r"C:\Program Files\X\unins000.exe";
    let c = parse_uninstall(r"C:\Program Files\X\unins000.exe /S", &e).unwrap();
    assert_eq!(c.file, r"C:\Program Files\X\unins000.exe"); assert_eq!(c.args, "/S");
    let c = parse_uninstall(r#""C:\a b\u.exe" --x"#, &|_| true).unwrap(); assert_eq!(c.args, "--x");
    let c = parse_uninstall("MsiExec.exe /X{ABC}", &|_| false).unwrap(); assert_eq!(c.args, "/X{ABC}");
}
#[test] fn no_leftovers_while_still_registered() {
    assert!(matches!(leftover_state("k", &|_| true), LeftoverState::StillInstalled));
    assert!(matches!(leftover_state("k", &|_| false), LeftoverState::Ready));
}
#[test] fn list_installed_on_this_machine_returns_entries() { /* hanya membaca */ }
```

- [ ] **Step 2: Pastikan gagal:** `cargo test -p katharistis-core apps`.
- [ ] **Step 3: Implementasikan. Test tidak pernah menjalankan `run_uninstaller`.**
- [ ] **Step 4: Pastikan lolos.**
- [ ] **Step 5: Commit:** `feat(core): list installed apps and launch uninstallers`

---

### Task 10: winget

**Files:**
- Create: `crates/core/src/apps/winget.rs`, `crates/core/tests/fixtures/winget_list.txt`, `crates/core/tests/fixtures/winget_upgrade.txt`

**Interfaces:**
- Consumes: `AppEntry`.
- Produces:
```rust
pub struct WingetPackage { pub name: String, pub id: String, pub version: String, pub available: Option<String> }
pub enum WingetOutcome { Ok(Vec<WingetPackage>), NeedsAgreement(String), NotInstalled, Failed(String) }
pub fn parse_table(output: &str) -> Vec<WingetPackage>;
pub fn is_framework(id: &str) -> bool;
pub fn list(accept_agreements: bool) -> WingetOutcome;
pub fn upgrades(accept_agreements: bool) -> WingetOutcome;
pub fn run_visible(args: &[&str]) -> io::Result<i32>; // uninstall/upgrade di jendela konsol yang terlihat
pub enum AppRow { Registry(AppEntry, Option<WingetPackage>), WingetOnly(WingetPackage) }
pub fn merge(entries: Vec<AppEntry>, packages: Vec<WingetPackage>) -> Vec<AppRow>;
```

Keputusan:
- Keluaran winget mengikuti bahasa Windows dan tidak punya format JSON. `parse_table` tidak membaca nama kolom. Ia mencari baris pemisah `---`, mengambil posisi kolom dari baris judul tepat di atasnya, lalu memotong per lebar tampilan karakter. Kolom urut: nama, id, versi, tersedia (kalau ada lima kolom), sumber. Karakter `\r` dan spinner sebelum tabel dibuang.
- `merge` mencocokkan id `ARP\...\<key>` dengan `AppEntry::key`, lalu nama yang dinormalkan. Nama yang terpotong `…` dicocokkan sebagai prefiks.
- `is_framework`: awalan `Microsoft.VCLibs`, `Microsoft.NET`, `Microsoft.UI.Xaml`, `Microsoft.WindowsStore`, `Microsoft.Windows`, `Microsoft.DesktopAppInstaller`, juga dengan awalan `MSIX\`.
- `list(false)` tidak mengirim `--accept-source-agreements`. Stdin kosong. Keluaran yang gagal dan memuat "agreement" atau kode keluar agreement menjadi `NeedsAgreement(teks)`. GUI menampilkan teksnya, dan baru memanggil `list(true)` setelah pengguna menyetujui.
- `Command` memakai `CREATE_NO_WINDOW` untuk list. `NotFound` menjadi `NotInstalled`.

- [ ] **Step 1: Rekam fixture nyata dari mesin ini:** `winget list --disable-interactivity > fixtures/winget_list.txt` dan `winget upgrade --disable-interactivity > fixtures/winget_upgrade.txt`. Ini hanya membaca. Anonimkan bila perlu.
- [ ] **Step 2: Tulis test yang gagal**

```rust
#[test] fn parses_recorded_output() { assert!(parse_table(include_str!("../../tests/fixtures/winget_list.txt")).len() > 3); }
#[test] fn parses_localized_headers_and_truncated_names() {
    let s = "\r- \rNama    Id       Versi Tersedia Sumber\n-----------------------------------\nVisual…  Foo.Bar  1.0   1.1      winget\n";
    let p = &parse_table(s)[0];
    assert_eq!(p.id, "Foo.Bar"); assert_eq!(p.available.as_deref(), Some("1.1"));
}
#[test] fn framework_filter_handles_msix_prefix() { assert!(is_framework(r"MSIX\Microsoft.VCLibs.x64_1")); assert!(!is_framework("Mozilla.Firefox")); }
#[test] fn merge_by_arp_key_then_name() {}
#[test] fn empty_or_garbage_output_yields_no_packages() {}
```

- [ ] **Step 3: Pastikan gagal:** `cargo test -p katharistis-core winget`.
- [ ] **Step 4: Implementasikan `winget.rs`.**
- [ ] **Step 5: Pastikan lolos.**
- [ ] **Step 6: Commit:** `feat(core): integrate winget list, upgrades, and merging`

---

### Task 11: Sisa uninstall dan tanda "dipakai aplikasi"

**Files:**
- Create: `crates/core/src/apps/leftovers.rs`, `crates/core/src/apps/marks.rs`

**Interfaces:**
- Consumes: `AppEntry`, `protect::canonical_key`, `win`.
- Produces:
```rust
pub fn normalize_app_name(name: &str) -> String;
pub enum Certainty { Sure, Maybe }
pub struct Leftover { pub path: PathBuf, pub size: u64, pub certainty: Certainty }
pub struct LeftoverRoots { pub roaming: PathBuf, pub local: PathBuf, pub program_data: PathBuf }
pub fn find_leftovers(app: &AppEntry, roots: &LeftoverRoots, others: &[AppEntry]) -> Vec<Leftover>;

pub struct UsageMarks { /* canonical keys */ }
impl UsageMarks {
    pub fn new(apps: &[AppEntry], browser_profile_roots: Vec<PathBuf>) -> Self;
    pub fn is_app_owned(&self, p: &Path) -> bool;
}
```

Keputusan:
- `normalize_app_name`: buang penanda arsitektur (`(x64)`, `x86`, `64-bit`), nomor versi di akhir, dan kurung kosong. Bandingkan tanpa beda huruf besar kecil.
- `Sure`: `<root>\<publisher>\<nama>`, atau folder sama dengan `install_location`.
- `Maybe`: `<root>\<nama>`, hanya kalau nama setelah normalisasi minimal 4 karakter.
- Folder yang tidak pernah ditawarkan: penerbit aplikasi ini, penerbit aplikasi di `others`, dan `Microsoft`, `Google`, `Mozilla`, `Packages`, `Programs`, `Temp`, `Local`, `Roaming`.
- Kalau folder memiliki nama sama dengan aplikasi lain di `others`, buang.
- Daftar ini tidak punya kolom tercentang. GUI selalu memulai tanpa centang.
- Akar profil browser: `User Data` Chrome dan Edge, `Firefox\Profiles`.

- [ ] **Step 1: Tulis test yang gagal**

```rust
#[test] fn normalizes_version_and_arch() { assert_eq!(normalize_app_name("7-Zip 23.01 (x64)"), "7-zip"); assert_eq!(normalize_app_name("Slack"), "slack"); }
#[test] fn publisher_folder_is_never_offered() {
    // app "Foo" penerbit "Google"; folder Local\Google\Chrome dan Local\Google ada
    assert!(find_leftovers(&foo, &roots, &[]).iter().all(|l| l.path.file_name().unwrap() != "Google"));
}
#[test] fn sure_requires_publisher_path_or_install_location() {}
#[test] fn name_only_match_is_maybe_and_short_names_skipped() { /* "Slack" Maybe; "VLC" tanpa penerbit tidak ditemukan */ }
#[test] fn other_apps_folders_are_excluded() {}
#[test] fn marks_inside_install_location_and_browser_profiles() {}
#[test] fn marks_do_not_match_prefix_siblings() { /* C:\Apps\Foo tidak menandai C:\Apps\FooBar */ }
```

- [ ] **Step 2: Pastikan gagal:** `cargo test -p katharistis-core -- leftovers marks`.
- [ ] **Step 3: Implementasikan keduanya.**
- [ ] **Step 4: Pastikan lolos.**
- [ ] **Step 5: Commit:** `feat(core): find uninstall leftovers and app-owned marks`

---

### Task 12: Pengaturan dan kerangka GUI

**Files:**
- Create: `crates/core/src/settings.rs`, `crates/gui/src/{app,i18n,jobs}.rs`, `crates/gui/src/views/settings.rs`
- Modify: `crates/gui/src/main.rs`

**Interfaces:**
- Consumes: `ScanProgress`, `format_size`.
- Produces:
```rust
pub enum Language { Auto, English, Indonesian }  pub enum Theme { System, Light, Dark }
pub struct Settings { pub language: Language, pub theme: Theme, pub stale_days: u32, pub excluded: Vec<PathBuf> }
// Default: Auto, System, 365, kosong
pub fn load_from(path: &Path) -> Settings;              // rusak atau hilang → default, tanpa panic
pub fn save_to(path: &Path, s: &Settings) -> io::Result<()>; // tulis ke .tmp lalu rename
pub fn default_path() -> PathBuf;

// gui
pub fn l(en: &'static str, id: &'static str) -> &'static str; // i18n.rs, membaca bahasa aktif
pub fn pick(lang_id: u16) -> Language;                        // i18n.rs, Indonesian kalau lang_id & 0x3FF == 0x21
pub struct Job<T>; impl<T: Send + 'static> Job<T> {
    pub fn spawn(ctx: egui::Context, f: impl FnOnce() -> T + Send + 'static) -> Self;
    pub fn try_take(&mut self) -> Option<T>;
}
pub enum Tool { Home, Clean, Apps, Large, Stale, Dupes, Settings }
pub struct App { pub tool: Tool, pub settings: Settings, pub tree: Option<ScanTree>, /* ... */ }
```
- `#![windows_subsystem = "windows"]`. Menu samping: bagian Alat (Beranda, Pembersihan, Aplikasi) dan Analisis (Terbesar, Lama Tak Diubah, Duplikat), dengan drive dan sisa ruang di bawah, Pengaturan di paling bawah.
- `Auto` membaca `GetUserDefaultUILanguage` lalu memanggil `pick`.
- `settings.excluded` selalu diteruskan ke `ScanOptions::excluded` oleh semua pemindaian GUI.
- Warna mengikuti gaya CleanBro (latar gelap, aksen teal). Tema terang harus sama terbacanya.
- Gagal menyimpan pengaturan ditampilkan sebagai pesan, tidak menutup aplikasi.

- [ ] **Step 1: Tulis test yang gagal di `settings.rs` dan `i18n.rs`**

```rust
#[test] fn round_trips() {}
#[test] fn corrupt_file_gives_defaults() { /* tulis "not toml {{" → Settings::default(), stale_days 365 */ }
#[test] fn missing_file_gives_defaults() {}
#[test] fn language_auto_picks_indonesian_for_primary_0x21() { assert_eq!(pick(0x0421), Language::Indonesian); assert_eq!(pick(0x0409), Language::English); }
```

- [ ] **Step 2: Pastikan gagal:** `cargo test --workspace -- settings i18n`.
- [ ] **Step 3: Implementasikan pengaturan, kerangka jendela, navigasi, tema, halaman Pengaturan.**
- [ ] **Step 4: Pastikan lolos.**
- [ ] **Step 5: Jalankan:** `cargo run -p katharistis-gui`, lalu cek bahwa huruf `í` di judul tampil benar, navigasi jalan, dan pengaturan bahasa, tema tersimpan setelah aplikasi dibuka ulang.
- [ ] **Step 6: Commit:** `feat(gui): add window shell, settings, language, and theme`

---

### Task 13: Beranda, konfirmasi, dan laporan

**Files:**
- Create: `crates/gui/src/views/home.rs`, `crates/gui/src/confirm.rs`
- Modify: `crates/gui/src/app.rs`

**Interfaces:**
- Consumes: `scan`, `treemap::{squarify, map_items}`, `other_space`, `delete::{delete, DeleteMode, DeleteReport, Target}`, `recycle::{bin_info, empty_bin}`, `apps::marks::UsageMarks`, `protect::ProtectContext`.
- Produces (dipakai Task 14 sampai 16):
```rust
pub struct ConfirmRequest { pub items: Vec<Target>, pub mode: DeleteMode, pub warn_app_owned: usize, pub context: ConfirmContext }
pub enum ConfirmContext { User, Cleanup, Leftovers, Duplicates }
pub struct ConfirmSummary { pub count: usize, pub total: u64, pub permanent: bool, pub says_not_in_bin: bool, pub app_owned: usize }
pub struct ReportSummary { pub freed: u64, pub in_bin: u64, pub failed: usize, pub offer_empty_bin: bool }
pub fn confirm_summary(req: &ConfirmRequest) -> ConfirmSummary;
pub fn report_summary(r: &DeleteReport) -> ReportSummary;
pub fn not_recyclable(r: &DeleteReport) -> Vec<Target>;
pub enum DeleteFlow { Idle, Confirm(ConfirmRequest), Running(Job<DeleteReport>), ConfirmPermanent(Vec<Target>, DeleteReport), Report(DeleteReport) }
pub fn show_flow(ctx: &egui::Context, flow: &mut DeleteFlow, protect: &ProtectContext) -> Option<DeleteReport>; // mengembalikan laporan sekali saat selesai, untuk memperbarui pohon
```

Perilaku:
- Pemilih drive, default drive sistem. Tombol Pindai dan Batalkan, progres jumlah file dan ukuran.
- Treemap dari `map_items(tree, parent, 0.005, 40)` digambar dengan `egui::Painter`. Label hanya kalau muat. Warna per jenis area (profil, Program Files, Windows, lainnya), bukan tingkat bahaya. Hover menunjukkan nama lengkap, ukuran, tanggal.
- Klik folder masuk. Breadcrumb kembali. Kotak "Lainnya" membuka panel `other_space` di akar, dan membuka daftar lengkap di tingkat lain.
- Panel daftar: isi lokasi saat ini, terurut ukuran, kolom tanggal. Warna kuning kalau `modified` lebih tua dari `stale_days`. Tanda "di cloud" dan "dipakai aplikasi". Checkbox, awal selalu kosong. Item terlindungi tanpa checkbox.
- Tombol Pindahkan ke Recycle Bin dan opsi Hapus permanen.
- Dialog konfirmasi menunjukkan jumlah item, total ukuran, dan mode. Kalau ada item `dipakai aplikasi`, tampilkan peringatan dan jumlahnya.
- Laporan menampilkan "Dipindahkan ke Recycle Bin, belum mengosongkan disk: X", "Ruang yang didapat: Y", dan gagal beserta alasannya. Ada tombol **Kosongkan Recycle Bin**, dengan konfirmasi sendiri.
- `NotRecyclable` memunculkan langkah khusus: "File ini tidak bisa masuk Recycle Bin. Hapus permanen?" sebagai konfirmasi terpisah per kelompok, tanpa pilihan awal.
- Setelah selesai, `tree.remove_subtree` untuk yang hilang, dan ruang kosong disegarkan.

- [ ] **Step 1: Tulis test yang gagal di `confirm.rs`**

```rust
#[test] fn report_separates_bin_from_freed() {
    let s = report_summary(&DeleteReport { bytes_freed: 10, bytes_in_bin: 100, ..Default::default() });
    assert_eq!(s.freed, 10); assert_eq!(s.in_bin, 100); assert!(s.offer_empty_bin);
}
#[test] fn not_recyclable_is_offered_separately_and_nothing_else_is() {
    // failed: NotRecyclable, InUse → not_recyclable hanya mengembalikan yang pertama
}
#[test] fn cleanup_confirmation_states_permanent() {
    let s = confirm_summary(&req(DeleteMode::Permanent, ConfirmContext::Cleanup));
    assert!(s.permanent); assert!(s.says_not_in_bin);
}
#[test] fn app_owned_warning_counts_items() {}
```

- [ ] **Step 2: Pastikan gagal:** `cargo test -p katharistis-gui confirm`.
- [ ] **Step 3: Implementasikan `confirm.rs` dan `views/home.rs`.**
- [ ] **Step 4: Pastikan lolos.**
- [ ] **Step 5: Uji nyata di folder uji buatan**, bukan data asli: buat `%TEMP%\katharistis-uji` berisi file besar lewat `fsutil file createnew` dan pindai folder itu. Cek bahwa klik masuk dan breadcrumb jalan, tanggal lama berwarna, file masuk Recycle Bin, laporan memisahkan angkanya. Jangan tekan Kosongkan Recycle Bin kecuali isinya sudah dicek bersama pengguna. Kemudian pindai `C:` sungguhan dan catat waktunya.
- [ ] **Step 6: Commit:** `feat(gui): add disk map, confirmation, and report flow`

---

### Task 14: Layar Pembersihan

**Files:**
- Create: `crates/gui/src/views/clean.rs`

**Interfaces:**
- Consumes: `junk::{scan_junk, JunkRoots, JunkCategory}`, `confirm::{ConfirmRequest, ConfirmContext::Cleanup, show_flow}`, `DeleteMode::Permanent`, `recycle::empty_bin`.
- Produces: `pub fn selected_targets(cats: &[JunkCategory]) -> (Vec<Target>, bool /* kosongkan bin */)`.

Kategori dengan centang tiga keadaan, baris bisa dibuka untuk item. Judul menunjukkan total terpilih. Selalu permanen, konfirmasi menyebut item tidak masuk Recycle Bin. Recycle Bin hanya dikosongkan kalau dicentang. File yang sedang dipakai masuk laporan sebagai "dilewati, sedang dipakai", tidak sebagai kesalahan.

- [ ] **Step 1: Tulis test yang gagal**

```rust
#[test] fn selection_uses_item_flags_and_bin_only_when_checked() {}
#[test] fn unchecked_category_contributes_nothing() {}
```

- [ ] **Step 2: Pastikan gagal:** `cargo test -p katharistis-gui clean`.
- [ ] **Step 3: Implementasikan.**
- [ ] **Step 4: Pastikan lolos.**
- [ ] **Step 5: Jalankan** pemindaian di mesin ini tanpa menghapus. Cek daftar, dan bahwa `.cache` serta Recycle Bin tidak tercentang.
- [ ] **Step 6: Commit:** `feat(gui): add cleanup screen`

---

### Task 15: Layar Terbesar, Lama Tak Diubah, Duplikat

**Files:**
- Create: `crates/gui/src/views/{large,stale,dupes}.rs`

**Interfaces:**
- Consumes: `analysis::{large_files, stale_files, LARGE_MIN}`, `dupes::{find_duplicates, targets_still_safe, DuplicateGroup}`, `show_flow`.
- Produces: `pub struct DupeSelection` dengan `toggle(&mut self, group: usize, path: &Path)` yang menolak `keep`.

Terbesar dan Lama memakai pohon dari Beranda kalau sudah ada, kalau belum menawarkan pindai. Lama memiliki pilihan 6 bulan, 1 tahun, 2 tahun. Duplikat berjalan di Job dengan progres dan pembatalan, `keep` tampil tanpa checkbox. Sebelum konfirmasi, `targets_still_safe` dijalankan per grup. Grup yang gagal dikeluarkan dan diberi tahu. Mode default Recycle Bin.

- [ ] **Step 1: Tulis test yang gagal**

```rust
#[test] fn keep_cannot_be_selected() {}
#[test] fn selection_clears_when_group_is_removed() {}
```

- [ ] **Step 2: Pastikan gagal:** `cargo test -p katharistis-gui dupes`.
- [ ] **Step 3: Implementasikan ketiga layar.**
- [ ] **Step 4: Pastikan lolos.**
- [ ] **Step 5: Uji nyata di folder uji** berisi file kembar dan satu hard link. Cek pengelompokan dan hapus satu salinan.
- [ ] **Step 6: Commit:** `feat(gui): add large, stale, and duplicate screens`

---

### Task 16: Layar Aplikasi

**Files:**
- Create: `crates/gui/src/views/apps.rs`

**Interfaces:**
- Consumes: `apps::registry::{list_installed, still_registered}`, `apps::uninstall::{parse_uninstall, run_uninstaller, leftover_state, LeftoverState}`, `apps::winget::*`, `apps::leftovers::find_leftovers`, `show_flow`.
- Produces: `pub enum AppsPhase { Idle, Uninstalling(String), Waiting(String), Leftovers(Vec<Leftover>) }` dengan `pub fn next_after_exit(key: &str, registered: &dyn Fn(&str) -> bool) -> AppsPhase`.

Perilaku:
- Saat tab dibuka hanya registry. Kolom sesuai spec, urut ukuran, kotak cari.
- Tombol **Periksa winget**, disembunyikan kalau `NotInstalled`. `NeedsAgreement` menampilkan teks persetujuan dan tombol Setujui. Tidak ada persetujuan otomatis.
- Uninstall meminta konfirmasi, lalu menjalankan uninstaller resmi atau `winget uninstall --id X --exact` (`run_visible`). Pembaruan: `winget upgrade --id X --exact`, per aplikasi.
- Setelah proses keluar: kalau masih terdaftar, tampilkan "Uninstall belum selesai atau dibatalkan" dengan tombol **Periksa lagi**. Kalau tidak terdaftar, cari sisa file dan tampilkan daftar tanpa centang. Penghapusan sisa memakai `ConfirmContext::Leftovers` ke Recycle Bin.
- Aplikasi tanpa `uninstall` menampilkan pesan dan tidak bisa di-uninstall dari sini.
- Pemanggilan uninstaller dan winget uninstall di uji manual hanya pada aplikasi kecil yang dipilih bersama pengguna.

- [ ] **Step 1: Tulis test yang gagal**

```rust
#[test] fn stays_waiting_while_registered() { assert!(matches!(next_after_exit("k", &|_| true), AppsPhase::Waiting(_))); }
#[test] fn moves_on_when_unregistered() {}
```

- [ ] **Step 2: Pastikan gagal:** `cargo test -p katharistis-gui apps`.
- [ ] **Step 3: Implementasikan.**
- [ ] **Step 4: Pastikan lolos.**
- [ ] **Step 5: Jalankan** dan cek daftar serta tombol winget, tanpa menghapus aplikasi apa pun.
- [ ] **Step 6: Commit:** `feat(gui): add apps screen with winget and leftovers`

---

### Task 17: Rilis dan pemeriksaan akhir

**Files:**
- Modify: `README.md`, `Cargo.toml`
- Create: `crates/gui/assets/icon.ico`, `crates/gui/build.rs`

**Interfaces:**
- Consumes: semua.

- [ ] **Step 1: Tambah ikon dan metadata exe lewat `winresource`, plus manifest `asInvoker`.**
- [ ] **Step 2: Lengkapi README:** cara pakai, tabel opsi CLI (dengan catatan `-x`), lokasi yang dibersihkan, peringatan SmartScreen, atribusi.
- [ ] **Step 3: Verifikasi:** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. Semua harus lolos.
- [ ] **Step 4: Bangun rilis:** `cargo build --release --workspace`. Pastikan `target\release\katharistis-gui.exe` dan `katharistis.exe` berjalan dari folder lain tanpa file pendamping.
- [ ] **Step 5: Jalani kriteria berhasil pada folder uji:** peta, turun ke file besar lama, Recycle Bin, laporan terpisah, kosongkan Recycle Bin (setelah isi dicek bersama pengguna), ruang kosong bertambah.
- [ ] **Step 6: Pastikan tanpa jaringan:** cari `reqwest`, `ureq`, `hyper`, `TcpStream`, `WinHttp` di `Cargo.lock` dan source. Hasilnya harus kosong.
- [ ] **Step 7: Commit:** `chore: prepare release build and docs`
