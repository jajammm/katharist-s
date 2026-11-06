# Katharistís

Pembersih disk untuk Windows, ditulis dengan Rust. Menampilkan peta disk, lalu Anda sendiri yang memutuskan apa yang dihapus. Tidak ada yang tercentang otomatis dari berkas pribadi Anda.

Dua program, keduanya portabel (satu `.exe`, tanpa installer, tanpa hak admin):

- `katharistis-gui.exe`: aplikasi dengan antarmuka grafis.
- `katharistis.exe`: laporan penggunaan disk di terminal. Hanya membaca, tidak pernah menghapus.

## Cara pakai

Bangun dengan `cargo build --release --workspace`. Hasilnya ada di `target\release\`.

Aplikasi grafis:

1. Jalankan `katharistis-gui.exe`, pilih drive (atau ketik folder), tekan **Scan**.
2. **Home** menampilkan peta disk. Klik kotak untuk masuk lebih dalam, pakai jejak lokasi di atas untuk kembali. Item yang dicentang dipindahkan ke Recycle Bin.
3. **Cleanup** membersihkan cache dan berkas sementara (daftar di bawah). Item di sini dihapus permanen, dan konfirmasinya menyebut hal itu.
4. **Apps** mendaftar aplikasi terpasang, menjalankan uninstaller resminya, lalu menawarkan sisa folder untuk dicentang. Tidak ada sisa yang tercentang awal.
5. **Largest**, **Not changed in a while**, dan **Duplicates** membantu mencari berkas besar, lama, dan kembar. Satu salinan dalam tiap kelompok duplikat selalu tersisa.
6. **Settings** mengatur bahasa (Indonesia atau Inggris), tema, ambang "sudah lama", dan folder yang dikecualikan.

Berkas yang dipindahkan ke Recycle Bin belum mengembalikan ruang disk. Laporan memisahkan "dipindahkan ke Recycle Bin" dari "ruang yang benar-benar didapat", dan ada tombol untuk mengosongkan Recycle Bin.

Berkas yang tidak bisa masuk Recycle Bin (melebihi kuota, drive tanpa Recycle Bin, atau folder yang berisi link) tidak pernah dihapus begitu saja. Hapus permanen hanya terjadi setelah konfirmasi terpisah.

## CLI

    katharistis [opsi] [path]

Tanpa `path`, yang dipindai adalah folder profil pengguna.

| Opsi | Arti | Bawaan |
|---|---|---|
| `-d <DEPTH>` | Kedalaman pohon yang ditampilkan | 1 |
| `-n <TOP>` | Jumlah folder teratas per tingkat (0 = semua) | 20 |
| `-f <FILES>` | Jumlah berkas terbesar yang didaftar (0 = mati) | 15 |
| `-x` | Diterima demi kompatibilitas, tidak mengubah apa pun, karena pemindaian tidak pernah mengikuti link | |
| `-j <THREADS>` | Jumlah thread pemindai | jumlah CPU x 4 |

## Lokasi yang dibersihkan

Semua di bawah akun pengguna sendiri. Lokasi yang butuh hak admin (`C:\Windows\Temp`, cache Windows Update) tidak termasuk.

| Kategori | Lokasi | Tercentang awal |
|---|---|---|
| File sementara | Isi `%TEMP%` yang tidak diubah dalam 24 jam terakhir tidak disentuh | Ya |
| Cache browser | Folder `Cache` dan `Code Cache` per profil Chrome dan Edge, dan `cache2` per profil Firefox. Profil, sandi, dan bookmark tidak disentuh | Ya |
| Crash dump dan log | `%LOCALAPPDATA%\CrashDumps` dan `%LOCALAPPDATA%\Microsoft\Windows\WER` | Ya |
| Data developer | npm cache, pip cache, Gradle caches, NuGet `v3-cache` | Ya |
| Cache umum | `%USERPROFILE%\.cache` | Tidak |
| Unduhan tidak lengkap | `.crdownload`, `.part`, `.partial` di `Downloads` | Ya |
| Recycle Bin | Dikosongkan lewat API Windows | Tidak |

Berkas yang sedang dipakai dilewati dan dilaporkan.

## Kecepatan

Satu pemindaian penuh satu drive berisi sekitar 3,6 juta berkas memakan sekitar 5,6 menit, diukur di mesin pengembang. Mesin lain akan berbeda.

## Jaringan

Katharistís tidak memakai jaringan, tidak ada telemetri. Satu-satunya pengecualian adalah `winget` di layar Apps, yang baru dijalankan setelah Anda menekan tombolnya.

## Peringatan SmartScreen

Binary tidak ditandatangani, jadi Windows SmartScreen bisa menampilkan peringatan saat pertama dijalankan. Pilih "More info" lalu "Run anyway" bila Anda mempercayai sumbernya.

## Asal dan lisensi

Port dari [CleanBro](https://github.com/jetorbitdev/cleanbro), Copyright (C) 2026 gilang. Perilaku dan struktur mengikuti proyek itu.

Dilisensikan di bawah GPL-3.0. Lihat berkas `LICENSE`.
