# AudioSplit

Windows 10 və Windows 11 üçün virtual audio kabel tələb etməyən, quraşdırmasız, çox kiçik və portativ multi-output səs yönləndiricisi.

Kompüterin cari default səs çıxış cihazındakı səsi **WASAPI loopback** vasitəsilə tutur və eyni anda seçdiyiniz bir neçə çıxış cihazına (dinamik, qulaqlıq, HDMI/DisplayPort monitor, Bluetooth və s.) paralel göndərir.

## Skrinşot

![AudioSplit əsas pəncərə](docs/screenshot.png)

> Qeyd: Skrinşot əlavə etmək üçün şəkli `docs/screenshot.png` olaraq yadda saxlayın.

## Əsas Xüsusiyyətlər

- **Virtual audio kabel yoxdur:** Birbaşa Windows Core Audio (WASAPI loopback + event-driven render) istifadə edir.
- **Tək portativ `.exe`:** Quraşdırma, administrator hüququ, əlavə DLL və ya Visual C++ Redistributable tələb etmir.
- **Səs döngüsü (feedback) qoruması:** Tutulan default çıxış cihazı interfeysdə deaktiv edilir və səs mühərrikində çıxış siyahısından sərt şəkildə kənarlaşdırılır.
- **Hər cihaz üçün müstəqil tənzimləmə:**
  - Aç/bağla (`Checkbox`)
  - Səs səviyyəsi (`0–100%`)
  - Gecikmə (`0–1000 ms`) — fərqli cihazları (xüsusən Bluetooth və HDMI) sinxronlaşdırmaq üçün
- **Saat sürüşməsi (clock drift) kompensasiyası:** Fərqli səs kartlarının saat fərqini avtomatik balanslaşdıran adaptiv gecikmə buferi.
- **Sistem tray ikonu və portativ konfiqurasiya:** Pəncərəni bağladıqda tray-a yığılır; seçimlər `.exe` faylının yanındakı `audiosplit.json` faylında saxlanılır.
- **Avtomatik cihaz yeniləməsi (`IMMNotificationClient`):** Cihaz qoşulub-ayrılanda və ya Windows default cihazı dəyişəndə proqram çökmədən siyahını və axınları yeniləyir.

## Necə İşlədilir (Son İstifadəçi Üçün)

1. GitHub **Releases** bölməsindən ən son `AudioSplit.exe` faylını yükləyin.
2. İstədiyiniz qovluğa qoyun və birbaşa işə salın — quraşdırma lazım deyil.
3. Siyahıdan səs göndərmək istədiyiniz əlavə cihazları işarələyin, səs səviyyəsini və gecikməni (`ms`) tənzimləyin.
4. Pəncərəni bağlasanız (`X`) və ya `Hide to Tray` düyməsini sıxsanız, proqram sistem tray-da arxa planda işləməyə davam edəcək. Tam çıxış üçün `Quit` düyməsini və ya tray sağ-klik menyusundan `Quit` seçin.

### Bluetooth Cihazlar Üçün Sinxronizasiya Məsləhəti

Bluetooth qulaqlıq və dinamiklərdə adətən 100–250 ms təbii gecikmə olur. Rəqəmsal olaraq "mənfi gecikmə" tətbiq etmək mümkün olmadığı üçün:
1. **Ən yavaş cihazı** (məsələn, Bluetooth qulaqlığı) Windows-da **Default** səs çıxış cihazı təyin edin.
2. **AudioSplit** siyahısında daha sürətli olan simli dinamik və ya HDMI monitorun qarşısında **100–250 ms** arası gecikmə dəyəri seçin.

Beləliklə, simli/HDMI çıxış Bluetooth qədər gözləyəcək və bütün cihazlar eyni anda sinxron eşidiləcək.

## Fayl Ölçüsü və Texniki Detallar

- **Real ölçülən binary ölçüsü** (`x86_64-pc-windows-gnu`, `opt-level = "z"`, `lto = true`, `strip = true`): **427,520 bayt (417.5 KB / 0.408 MB)** — 15 MB hədəfindən xeyli kiçikdir.
- Yalnız Rust və rəsmi `windows` crate-i ilə nativ Win32 UI + WASAPI üzərində qurulub; yalnız Windows 10/11-in standart sistem DLL-lərindən istifadə edir (xarici runtime DLL asılılığı yoxdur).
- GitHub Actions release pipeline-ı `x86_64-pc-windows-msvc` hədəfində statik CRT (`+crt-static`) ilə build edib `AudioSplit.exe` və `SHA256SUMS.txt` fayllarını Releases bölməsinə yükləyir.

## Antivirus və SmartScreen Xəbərdarlığı Barədə

`AudioSplit.exe` rəqəmsal imza (Code Signing sertifikatı) olmayan, kiçik ölçülü nativ Win32 icra faylıdır. Bu səbəbdən Windows Defender SmartScreen və ya bəzi antivirus proqramları onu səhvən **yalan xəbərdarlıq (false positive)** kimi işarələyə bilər.

Təhlükəsizlikdən əmin olmaq üçün:
- Bütün mənbə kodu açıqdır və yoxlanıla bilər.
- Releases bölməsindəki fayllar birbaşa bu repodakı GitHub Actions workflow-u ilə təmiz mühitdə qurulur və `SHA256SUMS.txt` ilə verifikasiya oluna bilər.
- İstəsəniz aşağıdakı təlimatla öz kompüterinizdə birbaşa mənbə kodundan build edə bilərsiniz.

## Məlum Məhdudiyyətlər

- Yalnız **Windows 10 / Windows 11 (x64)** əməliyyat sistemləri dəstəklənir.
- **WASAPI Exclusive Mode** ilə işləyən proqramlar (məsələn, bəzi DAW və ya eksklüziv rejimli musiqi pleyerləri) Windows audio mikserindən yan keçdiyi üçün loopback ilə tutula bilmir.
- Bəzi audio drayverlərdə Windows-un Default çıxış cihazının səsini `0%` etmək və ya Mute qoymaq loopback səsini də kəsə bilər.
- Gecikmə (`0–1000 ms`) yalnız əlavə çıxış cihazlarına tətbiq olunur; default cihazdan daha yavaş olan cihazı sürətləndirmək (mənfi gecikmə) fiziki olaraq mümkün deyil.

## Mənbədən Build Etmək

Tələb: [Rust](https://rustup.rs/) (Windows `x86_64-pc-windows-msvc` və ya `x86_64-pc-windows-gnu`).

```powershell
cargo test
cargo build --release
```

Hazır icra faylı: `target/release/AudioSplit.exe`

Avtomatik GitHub Release yaratmaq üçün tag push edin:

```bash
git tag v1.0.0
git push origin v1.0.0
```

## Lisenziya

[MIT License](LICENSE)
