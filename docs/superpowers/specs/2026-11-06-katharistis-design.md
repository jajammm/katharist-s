# Katharistís: dokumen desain

Tanggal: 2026-11-06
Sumber acuan: https://github.com/jetorbitdev/cleanbro (v1.1.1, GPL-3.0, Swift + Go, khusus macOS)

## Kenapa ini dibuat

Pagi ini disk pengguna penuh. Cara yang dipakai: buka Windows Storage, buka folder yang besar, lalu cek isinya satu per satu untuk mencari yang besar. Cara itu membuang waktu karena harus dibuka satu per satu.

Yang diinginkan: sekali pindai, langsung terlihat apa yang memakan tempat, lalu pengguna memutuskan sendiri apa yang dihapus.

Cara pengguna memutuskan:
- File dan folder: dihapus kalau sudah tidak dipakai. Petunjuk utamanya tanggal terakhir diubah.
- Aplikasi: tanggal saja tidak cukup. Aplikasi yang masih rajin update tetap dihapus kalau pengguna sudah tidak memerlukannya. Hanya pengguna yang bisa menilai.

Pengguna lebih mudah memahami tampilan visual, jadi peta disk adalah layar pembuka.

Cara pengguna memasang aplikasi: installer dari website, kadang winget.

## Kriteria berhasil

Dari membuka aplikasi sampai melihat isi terbesar disk, pengguna tidak perlu membuka satu folder pun secara manual. Dari peta, pengguna bisa turun ke file besar yang sudah lama, memindahkannya ke Recycle Bin, mengosongkan Recycle Bin dari laporan hasil, dan melihat ruang kosong bertambah.

## Keputusan pengguna

- Aplikasi Windows, ditulis dengan Rust.
- Layar pembuka berupa peta disk (treemap). Klik kotak untuk masuk lebih dalam. Dipilih dari tiga mockup.
- Semua fitur CleanBro ikut: pembersih cache, uninstall aplikasi, file duplikat, pengelola paket.
- Pengelola paket memakai winget, bukan padanan langsung Homebrew.
- Alat terminal (CLI) ikut.
- Pembersihan cache dan file sementara menghapus permanen, karena isinya bisa dibuat ulang oleh aplikasinya. Konfirmasinya menyebutkan hal ini dengan jelas.
- File dan folder pilihan dari peta disk masuk Recycle Bin secara default. Laporan memisahkan "dipindahkan ke Recycle Bin, belum mengosongkan disk" dari "ruang yang benar-benar didapat", dengan tombol Kosongkan Recycle Bin di dekatnya.
- File yang terlalu besar untuk Recycle Bin mendapat tawaran hapus permanen dengan konfirmasi khusus, bukan sekadar pesan gagal.

## Keputusan pembuat (bisa ditolak pengguna)

- Keputusan menghapus selalu di tangan pengguna. File pribadi tidak pernah tercentang otomatis.
- Di luar menu Pembersihan, penghapusan default ke Recycle Bin. Hapus permanen harus dipilih sendiri dan dikonfirmasi.
- Di pembersih, kategori aman boleh langsung tercentang. Recycle Bin dan `.cache` tidak. Ini sengaja berbeda dari versi Mac, yang mencentang semuanya.
- Antarmuka dua bahasa: Bahasa Indonesia dan Inggris. Default mengikuti bahasa Windows, bisa diganti di Pengaturan.
- Tema terang, gelap, atau mengikuti sistem.
- Tanpa koneksi internet dan telemetri. Satu-satunya pemanggilan jaringan adalah winget, dan hanya ketika pengguna memakai fiturnya.
- Hasil berupa file `.exe` portabel tanpa installer. Dua binary: `katharistis-gui.exe` dan `katharistis.exe` (CLI).
- Lisensi GPL-3.0 dengan pencantuman proyek asal, karena perilaku dan struktur mengikuti CleanBro.
- Nama aplikasi: Katharistís (keputusan pengguna). Nama file dan folder teknis tanpa aksen: `katharistis`.
- GUI memakai `egui`/`eframe`: satu `.exe` tanpa WebView, dan treemap mudah digambar sendiri.
- Tab Homebrew tidak disalin. Fungsinya dilebur ke menu Aplikasi.
- winget hanya berjalan setelah pengguna menekan tombolnya, supaya janji "tanpa jaringan" tetap benar.
- Tidak ada sisa file uninstall yang tercentang awal, dan folder penerbit tidak pernah ditawarkan.
- Aplikasi di luar `Program Files` diberi tanda peringatan, tidak diblokir.
- Aplikasi Microsoft Store hanya muncul lewat winget, dengan framework dan paket bawaan disaring.

## Non-goals

- Pembersih registry, "optimizer", atau peningkatan performa.
- Penghapusan terjadwal atau otomatis.
- Pembaruan otomatis aplikasi itu sendiri.
- Pemindaian lewat MFT NTFS. Butuh hak admin dan menambah kompleksitas. Bisa dipertimbangkan kalau pemindaian biasa terbukti terlalu lambat.
- Code signing. SmartScreen akan memberi peringatan, dan ini dijelaskan di README.
- Dukungan macOS atau Linux.
- Windows di bawah 10.

## Anti-goals (gagal walau secara teknis jalan)

- Menghapus sesuatu yang tidak dipilih sadar oleh pengguna.
- Membuat Windows, aplikasi, atau data pengguna rusak karena salah hapus.
- Menampilkan ukuran yang menyesatkan sehingga pengguna mengambil keputusan yang salah.
- Terlihat seperti cleaner penuh peringatan menakut-nakuti. Tidak ada skor "kesehatan PC" atau angka bahaya.

## Fitur

### Beranda: peta disk

- Pemilih drive. Default drive sistem.
- Satu baris ringkasan: sisa ruang dan total.
- Treemap ukuran isi. Warna membedakan jenis area, bukan tingkat bahaya.
- Klik kotak untuk masuk, ada jejak lokasi (breadcrumb) untuk kembali.
- Kotak kecil digabung jadi "Lainnya".
- Panel daftar isi dari lokasi saat ini, terurut ukuran, dengan tanggal terakhir diubah. Item yang sudah lama ditandai warna. Ambang default 1 tahun, bisa diganti di Pengaturan.
- Item di panel bisa dicentang, lalu dipindahkan ke Recycle Bin.
- Bagian "Lainnya" menjelaskan selisih antara total pindaian dan ruang terpakai: `pagefile.sys`, `hiberfil.sys`, `swapfile.sys`, System Volume Information, dan folder yang tidak terbaca beserta alasannya. Sebagai informasi, tidak bisa dihapus dari sini.

### Pembersihan

Kategori, tiap item tingkat atas bisa dicentang:

| Kategori | Lokasi | Tercentang awal |
|---|---|---|
| File sementara | `%TEMP%` | Ya |
| Cache browser | Folder `Cache` dan `Code Cache` per profil Chrome, Edge, Firefox. Bukan profilnya | Ya |
| Crash dump dan log | `%LOCALAPPDATA%\CrashDumps`, WER di `%LOCALAPPDATA%\Microsoft\Windows\WER` | Ya |
| Data developer | npm cache, pip cache, Gradle caches, NuGet `http-cache` | Ya |
| Cache umum | `%USERPROFILE%\.cache` | Tidak |
| Unduhan tidak lengkap | Di `Downloads`: `.crdownload`, `.part`, `.partial` | Ya |
| Recycle Bin | `SHQueryRecycleBin` / `SHEmptyRecycleBin` | Tidak |

Lokasi yang butuh admin (`C:\Windows\Temp`, cache Windows Update) tidak termasuk versi pertama.

Semua kategori di sini dihapus permanen, kecuali Recycle Bin yang dikosongkan lewat API Windows. Konfirmasi menyatakan bahwa item tidak masuk Recycle Bin.

File yang sedang dipakai dilewati dan dilaporkan, tidak dianggap kesalahan.

### Aplikasi

- Daftar dari registry: `HKLM` dan `HKCU` `...\Windows\CurrentVersion\Uninstall`, termasuk `WOW6432Node`. Komponen sistem dan entri tanpa nama disembunyikan.
- Saat tab dibuka, hanya data registry yang dimuat, tanpa jaringan. winget baru berjalan ketika pengguna menekan "Periksa winget", karena winget bisa menyegarkan sumbernya lewat internet. Kalau winget meminta persetujuan sumber, persetujuan itu ditampilkan ke pengguna dan tidak diterima otomatis.
- Hasil winget digabung ke daftar: aplikasi yang dikenal winget diberi penanda, dan kolom pembaruan terisi. Kalau winget tidak terpasang, tombolnya disembunyikan tanpa pesan kesalahan.
- Aplikasi Microsoft Store hanya muncul lewat winget. Framework dan paket bawaan Windows (awalan `Microsoft.VCLibs`, `Microsoft.NET`, `Microsoft.UI.Xaml`, Store, dan sejenisnya) disaring. Ukuran dan tanggal pasangnya boleh kosong.
- Kolom: nama, penerbit, ukuran, tanggal pasang, versi, ketersediaan pembaruan.
- Uninstall menjalankan uninstaller resmi aplikasi (`QuietUninstallString` tidak dipakai, supaya pengguna melihat prosesnya), atau `winget uninstall` untuk aplikasi winget.
- Setelah uninstall selesai, aplikasi mencari sisa file di `%APPDATA%`, `%LOCALAPPDATA%`, `%PROGRAMDATA%`, lalu menampilkannya untuk dicentang pengguna. Tidak ada sisa file yang tercentang awal, karena folder ini bisa berisi data pribadi seperti save game dan proyek.
  - Nama aplikasi dinormalkan dulu: versi dan penanda arsitektur dibuang, jadi "7-Zip 23.01 (x64)" menjadi "7-Zip".
  - Tanda "pasti": nama folder sama dengan nama aplikasi **dan** berada di bawah folder penerbit, atau folder itu terdaftar di entri registry aplikasi.
  - Tanda "mungkin": nama folder sama dengan nama aplikasi saja.
  - Folder penerbit itu sendiri (`%LOCALAPPDATA%\Google`, `%APPDATA%\Microsoft`) tidak pernah ditawarkan, karena bisa berisi data aplikasi lain seperti profil Chrome.
  - Ini memperbaiki kelemahan versi Mac, yang mencocokkan nama saja sebagai "pasti".
- Pembaruan: daftar `winget upgrade`, perbarui per aplikasi atas perintah pengguna.
- Alur uninstall dengan hak admin memakai prompt UAC dari installer aplikasi itu sendiri. Katharistís sendiri berjalan tanpa hak admin.

### Analisis

- **Terbesar:** file 100 MB ke atas di lokasi yang dipilih.
- **Lama tak diubah:** file yang tidak diubah sejak ambang tertentu (6 bulan, 1 tahun, 2 tahun).
- **Duplikat:** kelompokkan per ukuran, lalu hash SHA-256. Satu salinan dalam tiap kelompok selalu tersisa dan tidak bisa dicentang. Hard link tidak dihitung sebagai duplikat.

### Pengaturan

Bahasa, tema, ambang "sudah lama", folder yang dikecualikan dari pemindaian. Disimpan di `%APPDATA%\Katharistis\settings.toml`.

### CLI `katharistis.exe`

Hanya baca. Tidak pernah menghapus.

`katharistis [opsi] [path]`, default ke folder profil pengguna.

| Opsi | Default | Arti |
|---|---|---|
| `-d` | 1 | Kedalaman pohon |
| `-n` | 20 | Jumlah folder teratas per level, `0` = semua |
| `-f` | 15 | Jumlah file terbesar, `0` = mati |
| `-x` | false | Jangan pindah ke volume lain |
| `-j` | CPU × 4 | Jumlah thread |

Keluaran mengikuti versi Go: bar ukuran, persentase, sisa ruang volume, peringatan untuk folder yang ditolak aksesnya.

## Aturan pemindaian

- Ukuran adalah ruang disk nyata, bukan ukuran logis. Pakai `GetCompressedFileSizeW`, supaya file terkompresi dan sparse benar.
- File placeholder OneDrive yang belum terunduh dihitung sebesar ruang yang benar-benar dipakai, dan diberi tanda "di cloud".
- Hard link dihitung sekali, memakai file ID dan jumlah link.
- Junction, symlink, dan mount point tidak diikuti. Reparse point jenis cloud (OneDrive) tetap dipindai tanpa memicu unduhan, karena kalau dilewati seluruh folder OneDrive tidak akan terhitung.
- Pemindaian tidak pernah membaca isi file di cloud, termasuk saat mencari duplikat.
- Tanggal terakhir diubah pada folder adalah tanggal terbaru dari seluruh isinya, karena tanggal bawaan folder tidak berubah saat isi di subfolder berubah.
- Folder yang gagal dibaca tidak menggagalkan pemindaian. Dicatat beserta alasannya dan ditampilkan.
- Paralel, bisa dibatalkan, hasil sebagian tetap terlihat. Progres ditampilkan selama pemindaian.
- Path panjang di atas 260 karakter didukung lewat awalan `\\?\`.

## Aturan keamanan penghapusan

Pemeriksaan di satu fungsi pustaka inti, berlaku untuk semua fitur.

Dilindungi, tidak pernah bisa dihapus:
- Akar drive, `C:\Windows` beserta isinya, `C:\Program Files`, `C:\Program Files (x86)`, `C:\ProgramData` sebagai folder, `C:\Users`, `System Volume Information`, `$Recycle.Bin`, `pagefile.sys`, `hiberfil.sys`, `swapfile.sys`.
- Folder profil pengguna dan folder standar di dalamnya sebagai folder utuh: `Documents`, `Desktop`, `Downloads`, `Pictures`, `Music`, `Videos`, `AppData`, `AppData\Local`, `AppData\Roaming`, `.ssh`, `.gnupg`. Isinya boleh dihapus, kecuali `.ssh` dan `.gnupg`.
- Folder Katharistís sendiri.

Aturan:
- Path dinormalkan dulu ke bentuk kanonik sebelum diperiksa, termasuk `\\?\`, huruf besar kecil, dan short name 8.3.
- Reparse point dihapus sebagai tautannya. Target tidak disentuh.
- Isi `Program Files` tidak dihapus langsung. Aplikasi dihapus lewat uninstaller.
- Item yang berada di dalam `InstallLocation` aplikasi terdaftar (termasuk `%LOCALAPPDATA%\Programs` dan drive lain) dan profil browser diberi tanda "dipakai aplikasi" di peta dan daftar, dengan peringatan di konfirmasi. Penghapusan tetap boleh, karena keputusan ada di tangan pengguna. Fungsi aturan path tetap murni. Tanda ini dihitung terpisah dari data aplikasi yang sudah dimuat.
- Sebelum menghapus selalu ada konfirmasi berisi jumlah item, total ukuran, dan mode hapus.
- Setelahnya ada laporan: terhapus, gagal beserta alasan, jumlah yang masih di Recycle Bin, dan ruang yang benar-benar didapat. Item di Recycle Bin tidak dihitung sebagai ruang yang didapat.
- Gagal pindah ke Recycle Bin tidak pernah otomatis menjadi hapus permanen. File yang terlalu besar untuk Recycle Bin ditawarkan untuk dihapus permanen, dan pengguna harus mengonfirmasi secara khusus.

## Struktur

Workspace Cargo:
- `core`: pemindaian, ukuran, treemap layout, keamanan path, penghapusan, junk scanner, pendeteksi aplikasi, duplikat. Tidak bergantung pada GUI.
- `gui`: antarmuka `egui`.
- `cli`: terminal.

## Pengujian

Versi Mac tidak punya test otomatis. Versi ini wajib punya:
- Unit test untuk aturan path terlindungi, termasuk bentuk `\\?\`, huruf besar kecil, dan short name.
- Test pada folder sementara: pemindaian dengan hard link, junction, dan folder tanpa izin baca.
- Test bahwa penghapusan menolak path terlindungi dan tidak mengikuti junction.
- Test duplikat dan treemap layout.
- Pemeriksaan manual di mesin ini dengan folder uji, bukan data asli pengguna.

## Yang diserahkan ke pembuat

Crate spesifik selain `egui`, palet warna, tata letak detail, ikon, penanganan performa treemap.
