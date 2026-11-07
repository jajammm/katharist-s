# Katharistís

Pembersih disk untuk Windows, ditulis dengan Rust. Sekali pindai, peta disk menunjukkan apa yang memakan tempat, jadi Anda tidak perlu membuka folder satu per satu. Keputusan menghapus tetap di tangan Anda: berkas pribadi tidak pernah tercentang otomatis.

Dua program, keduanya portabel (satu `.exe`, tanpa installer, tanpa hak admin), untuk Windows 10 ke atas:

- `katharistis-gui.exe`: aplikasi dengan antarmuka grafis, dalam Bahasa Indonesia dan Inggris.
- `katharistis.exe`: laporan penggunaan disk di terminal. Hanya membaca, tidak pernah menghapus.

## Status

Versi 0.1.0, belum ada rilis siap unduh. Test otomatis lolos, tetapi alur antarmuka grafis dengan data sungguhan masih menunggu uji manual, jadi cobalah dulu pada folder uji sebelum menghapus data penting.

## Membangun

```
cargo build --release --workspace
cargo test --workspace
```

Perlu Rust stable dengan target `x86_64-pc-windows-msvc` dan `rc.exe` dari Windows SDK. Hasil build ada di `target\release\`.

Test biasa tidak menyentuh Recycle Bin. Test yang memakai Recycle Bin sungguhan ditandai `#[ignore]` dan menaruh berkas uji `katharistis-test-*` ke sana, jadi jalankan hanya bila perlu:

```
cargo test -p katharistis-core -- --ignored --test-threads=1
```

## Fitur

### Beranda

Pilih drive atau ketik folder, lalu tekan **Pindai**. Peta disk menampilkan isi lokasi sebagai kotak. Klik kotak untuk masuk lebih dalam, dan pakai jejak lokasi untuk kembali. Warna kotak membedakan jenis area, tanpa skor bahaya.

Panel di sampingnya mendaftar isi lokasi saat ini dari yang terbesar, dengan tanggal terakhir diubah. Item yang sudah lama ditandai warna (ambang bawaan 1 tahun). Item yang dicentang dipindahkan ke Recycle Bin, sedangkan hapus permanen harus dipilih sendiri.

Pada pindaian satu drive, panel "Lainnya" menjelaskan selisih antara total pindaian dan ruang terpakai: `pagefile.sys`, `hiberfil.sys`, `swapfile.sys`, System Volume Information, dan folder yang tidak terbaca beserta alasannya.

### Pembersihan

Item di layar ini dihapus permanen karena bisa dibuat ulang oleh aplikasinya, dan konfirmasinya menyebut hal itu. Hanya lokasi di bawah akun pengguna sendiri yang dibersihkan. `C:\Windows\Temp` dan cache Windows Update tidak termasuk karena butuh hak admin.

| Kategori | Lokasi | Tercentang awal |
|---|---|---|
| File sementara | Isi `%TEMP%` | Ya, kecuali yang diubah dalam 24 jam terakhir |
| Cache browser | `Cache` dan `Code Cache` per profil Chrome dan Edge, `cache2` per profil Firefox. Profil, sandi, dan bookmark tidak disentuh | Ya |
| Crash dump dan log | `%LOCALAPPDATA%\CrashDumps`, `%LOCALAPPDATA%\Microsoft\Windows\WER` | Ya |
| Data developer | npm cache, pip cache, Gradle caches, NuGet `v3-cache` | Ya |
| Cache umum | `%USERPROFILE%\.cache` | Tidak |
| Unduhan tidak lengkap | `.crdownload`, `.part`, `.partial` di `Downloads` | Ya, kecuali yang diubah dalam 24 jam terakhir |
| Recycle Bin | Dikosongkan lewat API Windows | Tidak |

Berkas yang sedang dipakai dilewati dan dilaporkan. Folder yang dikecualikan di Pengaturan tidak ditawarkan.

### Aplikasi

Daftar aplikasi terpasang dibaca dari registry, dengan kolom nama, penerbit, ukuran, tanggal pasang, versi, dan pembaruan. Aplikasi di luar `Program Files` diberi tanda, tanpa diblokir.

Uninstall menjalankan uninstaller resmi aplikasi supaya prosesnya terlihat. Setelah aplikasi tidak lagi terdaftar, sisa foldernya di `%APPDATA%`, `%LOCALAPPDATA%`, dan `%PROGRAMDATA%` ditawarkan tanpa ada yang tercentang, karena bisa berisi data pribadi. Folder penerbit seperti `%LOCALAPPDATA%\Google` tidak pernah ditawarkan.

Tombol **Periksa winget** menambahkan penanda winget, aplikasi Microsoft Store, dan daftar pembaruan, yang dijalankan per aplikasi atas perintah Anda. Kalau winget meminta persetujuan sumber, teksnya ditampilkan dan hanya diterima setelah Anda menyetujuinya.

### Analisis

- **Terbesar:** berkas 100 MB ke atas di lokasi yang dipindai.
- **Lama Tak Diubah:** berkas yang tidak diubah sejak 6 bulan, 1 tahun, atau 2 tahun. Area Windows dan Program Files disembunyikan kecuali Anda menampilkannya.
- **Duplikat:** berkas 1 MiB ke atas dikelompokkan menurut ukuran, lalu hash SHA-256. Satu salinan tiap kelompok selalu tersisa dan diperiksa ulang tepat sebelum penghapusan. Hard link tidak dihitung sebagai duplikat.

### Pengaturan

Bahasa, tema (terang, gelap, atau mengikuti sistem), ambang "sudah lama", dan folder yang dikecualikan dari pemindaian. Disimpan di `%APPDATA%\Katharistis\settings.toml`.

### CLI

```
katharistis [opsi] [path]
```

Tanpa `path`, yang dipindai adalah folder profil pengguna.

| Opsi | Arti | Bawaan |
|---|---|---|
| `-d <DEPTH>` | Kedalaman pohon yang ditampilkan | 1 |
| `-n <TOP>` | Jumlah folder teratas per tingkat (0 = semua) | 20 |
| `-f <FILES>` | Jumlah berkas terbesar yang didaftar (0 = mati) | 15 |
| `-x` | Diterima tetapi tidak mengubah apa pun, karena pemindaian tidak pernah mengikuti link | |
| `-j <THREADS>` | Jumlah thread pemindai | jumlah CPU x 4 |

## Aturan pemindaian

- Ukuran adalah ruang disk nyata (`GetCompressedFileSizeW`), jadi berkas terkompresi dan sparse terhitung benar.
- Hard link dihitung sekali. Junction, symlink, dan mount point tidak diikuti.
- Folder OneDrive tetap dipindai tanpa memicu unduhan. Berkas yang masih di cloud diberi tanda dan isinya tidak pernah dibaca, termasuk saat mencari duplikat.
- Tanggal sebuah folder adalah tanggal terbaru dari seluruh isinya.
- Folder yang gagal dibaca dicatat beserta alasannya, dan pemindaian tetap berjalan.
- Pemindaian bisa dibatalkan, dan path di atas 260 karakter didukung.

Satu pemindaian penuh drive berisi sekitar 3,6 juta berkas memakan sekitar 5,6 menit di mesin pengembang.

## Aturan penghapusan

Semua penghapusan melewati satu fungsi di `katharistis-core`, dan selalu ada konfirmasi berisi jumlah item, total ukuran, dan mode hapus. Laporannya memisahkan berkas yang dipindahkan ke Recycle Bin, yang belum mengembalikan ruang disk, dari ruang yang benar-benar didapat. Ada tombol untuk mengosongkan Recycle Bin dari laporan itu, tanpa prompt Windows tambahan.

Berkas atau folder yang tidak bisa masuk Recycle Bin (melebihi kuota, drive tanpa Recycle Bin, atau folder yang berisi link) tidak pernah dihapus begitu saja. Hapus permanen hanya terjadi setelah konfirmasi terpisah.

Yang tidak bisa dihapus dari aplikasi ini:

- Akar drive, `C:\Windows` beserta isinya, `Program Files` beserta isinya, `C:\ProgramData` dan `C:\Users` sebagai folder, serta folder induk dari lokasi-lokasi itu.
- `System Volume Information`, `$Recycle.Bin`, `pagefile.sys`, `hiberfil.sys`, `swapfile.sys`.
- Berkas boot di akar drive Windows: `bootmgr`, `BOOTNXT`, `Boot`, `Recovery`, `EFI`.
- Folder profil tiap pengguna dan folder standarnya sebagai folder utuh (`Documents`, `Desktop`, `Downloads`, `Pictures`, `Music`, `Videos`, `AppData`, `AppData\Local`, `AppData\Roaming`). Isinya boleh dihapus.
- `.ssh` dan `.gnupg` beserta isinya, serta folder Katharistís sendiri.

Path dinormalkan dulu sebelum diperiksa, termasuk `\\?\`, huruf besar kecil, dan short name 8.3. Link dihapus sebagai tautannya, dan targetnya tidak disentuh. Item di dalam folder aplikasi terdaftar dan profil browser boleh dihapus, tetapi diberi tanda "dipakai aplikasi" dan peringatan di konfirmasi.

## Yang tidak dibuat

Pembersih registry, "optimizer", skor kesehatan PC, penghapusan terjadwal atau otomatis, pembaruan otomatis aplikasi ini, dan dukungan macOS atau Linux.

## Jaringan

Katharistís tidak memakai jaringan dan tidak punya telemetri. Satu-satunya pengecualian adalah `winget` di layar Aplikasi, yang baru dijalankan setelah Anda menekan tombolnya.

## Peringatan SmartScreen

Binary tidak ditandatangani, jadi Windows SmartScreen bisa menampilkan peringatan saat pertama dijalankan. Pilih "More info" lalu "Run anyway" bila Anda mempercayai sumbernya.

## Struktur

Workspace Cargo dengan tiga crate:

- `crates/core` (`katharistis-core`): pemindaian, treemap, aturan path, penghapusan, pembersihan, aplikasi, dan duplikat. Tidak bergantung pada GUI.
- `crates/gui` (`katharistis-gui`): antarmuka `egui`.
- `crates/cli` (`katharistis-cli`): program terminal.

Dokumen desain ada di [`docs/superpowers/specs/2026-11-06-katharistis-design.md`](docs/superpowers/specs/2026-11-06-katharistis-design.md), dan rencana pengerjaannya di [`docs/superpowers/plans/2026-11-06-katharistis.md`](docs/superpowers/plans/2026-11-06-katharistis.md).

## Asal dan lisensi

Port dari [CleanBro](https://github.com/jetorbitdev/cleanbro), Copyright (C) 2026 gilang. Perilaku dan struktur mengikuti proyek itu.

Dilisensikan di bawah GPL-3.0. Lihat berkas `LICENSE`.
