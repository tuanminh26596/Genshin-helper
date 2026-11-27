# Hồ sơ workflow agent: Genshin-Helper

Khung chung: `Docs/review-workflow.md` trong repo crew (commit `20eb3a9`). File này chỉ ghi phần riêng của
Genshin-Helper. **Chỉ người dùng được sửa** mục "Cấp tin cậy" và mục "Cửa một chiều".

## Mục tiêu

Yêu cầu gốc của người dùng (2026-11-26):

> hiện tại dự án đã phình to và bắt đầu phức tạp và tôi không thể nào ngồi test thật hết tất cả các tính năng mới ra. May mắn thay engine chính của chúng ta có thể hoạt động ngay cả khi không bật game mà có thể dùng video. Tôi nghĩ đã đến lúc chúng ta cần phải build một bộ test hay giả lập Khủng để cho các agent thay tôi làm công việt test thật đảm bảo đầu ra chính xác gần bằng hoặc như thực tế . tôi cần bạn suy luận phân tích sâu  để đưa ra kiến trúc/ giải pháp cho yêu cầu của tôi .

Diễn giải thành việc đo được: agent thay người dùng test thật bằng cách phát lại các bản quay qua đúng code
production (`run_tick_worker`) bằng `gh-sim`. "Đúng" nghĩa là kết quả replay khớp với baseline đã được người dùng
duyệt. Mỗi vùng tính năng chưa có phiên quay phủ thì chưa được tính là "đã test" (xem mục "Ngoài phạm vi gate").
Đích cuối: gate phủ đủ các tính năng để agent tự commit hoặc merge những thay đổi thuộc cửa hai chiều (khung §2).
## Cấp tin cậy hiện tại

`1` (làm trong worktree, người duyệt rồi commit). Đặt ngày 2026-11-27. Lý do: chưa có số liệu trong
`crew_scorecard`, vì việc đến giờ chạy trong IDE, không qua crew.

## Gate

Lệnh kiểm duy nhất và trung lập công cụ cho mọi agent hoặc người dùng: `pwsh -NoProfile -File scripts/check.ps1`. Mặc định (`-Tier auto`) tự chọn bước kiểm dựa trên các file đã đổi so với `main`, chạy từ rẻ đến đắt theo thứ tự cố định và dừng ngay ở lỗi đầu tiên. Dùng `-Tier full` trước khi gộp nhánh, `-Checks <list>` / `-QaOnly <session>` khi lặp nhanh, `-DryRun` để xem kế hoạch, `-SelfTest` để tự kiểm script, và thêm `-WebBench` khi cần chạy `web-bench`.

| Tên | Lệnh | Phạm vi | Trong `check.ps1` |
|---|---|---|---|
| guards | chặn `rust/target`, tăng `#[ignore]`, sửa `qa_baseline.json`/`verified_gt.json` (trừ `-AllowBaselineChange`) | toàn repo | luôn chạy |
| audit | `.\.venv\Scripts\python.exe scripts/audit_release.py --strict-ip` | toàn repo | luôn chạy |
| clippy | `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings` | `rust/**` | auto theo path |
| nextest | `cargo nextest run --manifest-path rust/Cargo.toml --workspace` | `rust/**` | auto theo path |
| frames | `gh-sim frames` (bộ fixture ảnh tĩnh trong `tests/frame_fixtures/*.json`: bố cục HUD 16:9/21:9 và định vị M-Map; chỉ `fail` chặn, `known-fail`/`xpass` chỉ báo) | `rust/crates/gh-sim/**`, `rust/crates/gh-core/src/ui_layout.rs`, `rust/crates/gh-cv/**`, `tests/frame_fixtures/**` | auto theo path |
| replay-qa | `gh-sim qa --sessions scratch\qa_sessions --data-dir data --ffmpeg <ABS>` (gate từng frame + độ giật mũi tên/nón; cột `Prov` kiểm tra diff trên `ENGINE_PROVENANCE_PATHS`, cột `Truth` kiểm tra độ chính xác tham khảo đối chiếu `tests/verified_gt.json`/anchor/vật lý; chạy các kịch bản trong `tests/scenarios`, chỉ `fail` làm qa thất bại, `known-fail` và `xpass` chỉ báo) | `rust/crates/gh-cv/**`, `rust/crates/gh-app/**`, `rust/crates/gh-sim/**`, `rust/crates/gh-core/src/ui_layout.rs`, `tests/scenarios/**` | auto theo path (`qa`) |
| e6 (đang `#[ignore]`) | lệnh chạy e6 kèm `--run-ignored`, cần xác nhận lại | `rust/crates/gh-cv/**` | chạy tay (`--run-ignored`) |
| web-bench | `pwsh -File scripts\web_bench.ps1 -Scenario scratch\web_scenario_all.json` (thêm `-Minimize` để mô phỏng tab bị ẩn sau game); cần tắt GenshinHelper vì dùng cổng 8765 | `extension/**`, `rust/crates/gh-net/**`, `rust/crates/gh-app/src/{tracking_loop,callbacks}.rs` | tùy chọn (`-WebBench`) |
| ext-smoke | `node tests/extension_dom_fixtures.mjs` + `node tests/extension_browser_smoke.mjs` (DOM popup HoYoLAB EN/VI trong `tests/fixtures/hoyolab_popup/**` và Chromium headless mô phỏng app; DOM chỉ `fail` chặn, `known-fail`/`xpass` chỉ báo) | `extension/**`, `tests/extension_browser_smoke.mjs`, `tests/extension_dom_fixtures.mjs`, `tests/fixtures/hoyolab_popup/**` | auto theo path |
| ext-i18n | `.\.venv\Scripts\python.exe tests/verify_i18n_and_ext_v5.py` | `extension/**` | auto theo path |
| blackbox | `.\.venv\Scripts\python.exe tests/test_blackbox_contracts.py --server-exe <exe> --host-exe <exe>` (80 test, 4 tầng: route, biên, phối hợp nhiều tính năng, kịch bản) | `rust/crates/gh-net/**`, `rust/crates/gh-app/**` | auto theo path |

`replay-qa` chạy khoảng 5,5 phút cho 12 phiên (đo 318 s ngày 2026-11-27). Nên giữ nó làm gate cho các glob CV,
không chạy cho mọi thay đổi. Thứ tự chạy theo mục "Test ladder" trong `AGENTS.md` và `CLAUDE.md`.

## Cửa một chiều cụ thể

| Đối tượng | Nhóm | Ràng buộc hiện tại | Lớp cứng nhất? |
|---|---|---|---|
| `scratch\qa_sessions\` (7 phiên, **là bản duy nhất**: `session_rec\` gốc đã bị xóa, thư mục này bị `.gitignore`, 4 phiên `libx265` đã mất màu so với bản gốc) | xóa dữ liệu | chỉ là lời dặn "tránh `git clean -fdx`" | **chưa**. Đề xuất: sao lưu sang ổ khác và đặt ACL chỉ cho đọc |
| `gh-sim qa --bless`, `blessed_poses.jsonl` | đổi đáp án đúng | mục `policy` trong `qa_baseline.json` | chưa. Đề xuất: gate so sha256 của baseline với một bản tham chiếu chỉ người được cập nhật |
| ngưỡng trong `tests/qa_baseline.json`, `tests/verified_gt.json` | nới ngưỡng, đổi đáp án đúng | mục `policy` | chưa. Đề xuất như trên |
| thêm `#[ignore]` hoặc skip test | nới ngưỡng | không có | chưa. Đề xuất: lint hoặc gate đếm số `#[ignore]` |
| GitHub Release, `SHA256SUMS.txt`, auto-updater | phát hành | thao tác tay | gần như đủ (người tự làm) |

## Bản đồ phủ test (thước đo "xong" của mục tiêu)

Agent chỉ được nói "đã kiểm như chơi thật" cho tính năng có mức **Đủ**. Mức **Unit** nghĩa là đã kiểm logic
từng phần, nhưng chưa có ai chạy nó như lúc chơi thật. Cập nhật bảng này mỗi khi thêm hoặc mất một lớp test.
Mỗi lỗi tìm thấy được ghi vào dev note, kèm ca test sẽ bắt được nó; không sửa ngay, trừ khi đó là lỗi của chính
bộ test.

| Tính năng | Kiểm bởi | Mức | Còn thiếu |
|---|---|---|---|
| Theo dõi minimap: vị trí, map | `qa` (gate từng frame, kèm cột `Truth` chỉ để xem), kịch bản `210432_global_relocate_post_teleport_known_bug` (`known_bug`), unit `gh-cv` | Đủ, có ngoại lệ | Replay lệch so với lúc chơi thật ở `082307` và `082625` (xem dev note). `011544` lệch vì bản live được quay bằng build cũ hơn bản sửa Frost Moon, không phải lỗi replay. Bản quay mới ghi commit, hash dữ liệu và chép bộ dữ liệu app đang dùng; replay dùng đúng bộ đó, cột `Prov` chỉ kiểm diff commit trên `ENGINE_PROVENANCE_PATHS` (`code=`/`code!`) và cột `Truth` đối chiếu `tests/verified_gt.json`, anchor `MAP_M_SYNCED` (`inliers >= 5`) / `layer_switch_sift` và giới hạn nhảy vật lý (`ok` / `susp ...`). 12 phiên hiện có chưa có provenance (`Prov` là `n/a`). Phiên `210432` lộ hạn chế của `TeyvatGlobalRelocator` sau dịch chuyển khi chạy với `terrain_n2_v4.npz` (0 phiếu trên `f217..f235`), đã khóa bằng kịch bản `known_bug`. Chưa có phiên cho vùng tối Mặt Trăng và Biển Trăng Dạt Dào |
| Hướng mũi tên, nón camera | `qa` (gate số lần giật so với baseline) | Đủ để đo | Lỗi đã biết: mũi tên bám theo nón. Chưa đo độ mượt của nón |
| Hầm, nhiều tầng | `qa` (phiên có map 9) | Một phần | Chưa có phiên nào đổi tầng trong hầm |
| M-Map: nhận diện, đồng bộ, khung nhìn | `qa` (số lần sync, số frame `mmap_view_acquired`), `frames` (`210311_f070/f233/f237` `known-fail` + `f146` `pass` và biến thể nhiễu `±2 LSB`) | Đủ cho kết quả | Bộ fixture ảnh tĩnh trong `tests/frame_fixtures/` đã khóa lỗi false-lock trên map 40 (2–3 inlier hoặc `prev_bonus`). Độ trễ lúc chơi thật chỉ có khi quay phiên mới (cột `HUD`). Đã sửa ở `c464c63` giữ mọi tick `Settling` và đếm tick bị bỏ (`Unrec`); 12 phiên cũ vẫn mang thiếu sót trước `c464c63` |
| Chỉ đường trên M-Map | `qa` (chỉ đếm `mmap_view`) | Một phần | Chưa kiểm hình chỉ đường vẽ ra |
| Đồng bộ vị trí lên web | `web-bench` (100 sự kiện, tab hiện và tab ẩn) | Đủ | Chưa đo trường hợp tab bị ẩn lâu hơn 5 phút (Chrome hãm hẹn giờ mạnh hơn) |
| Toast và bíp trong game | unit: luật quyết định (bắt được lỗi màn hình 2 khi gài lỗi), và mỗi `web_feedback` đều có một `hud_feedback` ghi game đã hiện gì. Cột `InGame` của `qa` kiểm trên bản quay thật | Unit; đo được trên bản quay mới | 12 phiên hiện có chưa có sự kiện `hud_feedback`, nên `InGame` là `n/a`. Cần quay phiên mới bằng bản build có thay đổi này. `web-bench` dùng callback giả, chưa chạy qua `on_action_feedback` thật. `hud_feedback` ghi việc app đã gửi toast và xếp hàng tiếng bíp, không phải pixel hay âm thanh đã ra thật |
| `Alt+F` đánh dấu pin | `ext-smoke` (`extension_dom_fixtures.mjs` chạy trực tiếp hàm tách từ `page_main.js` trên DOM HoYoLAB thật `vi`/`en`), `blackbox` (feedback, rollback), unit | DOM fixture + Unit | `tests/fixtures/hoyolab_popup/vi_unmarked.html` đạt `pass`, còn `en_unmarked.html` đạt `known-fail` do `"Mark Pin"` không khớp nhánh nút chính trong `page_main.js`; chờ duyệt sửa `extension/page_main.js`. Chưa có bench đánh dấu thật trên web |
| Nav HUD và các hotkey (`Alt+E/F6/T/H/F7/X`) | unit `gh-core` (`nav_controller`), `blackbox` (`nav_command`); replay ghi/khôi phục trạng thái ban đầu (`initial_state`, tách sai lệch trước web-input đầu bằng `(+<N>pre)`), dùng đồng hồ mô phỏng xác định (`AtomicU64` + tie-breaker theo `id`) để `qa`/`replay` lặp lại giống hệt giữa các lần chạy; 9 kịch bản trên `011805` đã phủ đủ `Alt+E`, `Alt+F6`, `Alt+F`, `Alt+T`, `Alt+H` (`nav_guide_toggle`), `Alt+F7` (`nav_layout_toggle`), `Alt+X` (`nav_toggle_hud`), `` Alt+` `` (`radar_toggle_and_edit`), `lock_target`, và `prev` qua `UiFrameSnapshot` | Đủ cho logic & trạng thái HUD | 12 phiên baseline chưa có pin thật nên chưa đối chiếu live từng frame; trên bộ 6 phiên mới, sai lệch `nav` hậu dịch chuyển trên `210432` phụ thuộc định vị toàn cục (`nav 41/709`). Replay không phát `sync`, `switch`, `debug` |
| Radar (`` Alt+` ``) | kịch bản `011805_radar_toggle_and_edit` (`UiFrameSnapshot`: `radar_enabled`/`radar_active`/`radar_edit_mode`), compositor headless trong `gh-sim`, unit `gh-hud` | Một phần | Chưa so pixel ảnh radar với ảnh chuẩn |
| Chuyển cửa sổ `Alt+Q`, chặn phím | unit `gh-win32` | Unit | Cần game thật, ngoài tầm replay |
| Chụp màn hình (GDI, WGC, HDR) | unit | Unit | Replay đi vòng qua phần chụp. Cần chơi thật |
| Bố cục HUD (`ui_layout`: 16:9 & 21:9) | `frames` (`16x9_1920x1080_control` `pass`, `21x9_2560x1080_hud_shift` `known-fail`), unit `gh-core` | Có kiểm tra định lượng | Chế độ ultrawide 21:9 (`2560×1080`) dịch cụm HUD minimap vào trong `+72 px` theo trục X; fixture đã khóa rõ độ lệch để chờ sửa `ui_layout.rs` |
| Bộ ghi phiên | unit `gh-app` (gồm `initial_state`, `provenance` với runtime `CARGO_MANIFEST_DIR` + `.git/HEAD` fallback, chép bộ dữ liệu, giữ tick `Settling`, đếm tick bị bỏ), unit `gh-sim compact` | Unit + kiểm lặp lại replay | Cần quay phiên mới bằng build có `initial_state` để kiểm chứng `nav_carried_over = 0` trên dữ liệu thật ngoài phiên tổng hợp |
| Extension: i18n, popup, khởi động trang | `ext-i18n`, `ext-smoke` | Đủ | Không cần thêm |
| API sync server, nhiều tab, LAN companion, nhập tiến độ từ map khác | `blackbox` tầng 1–4, unit `gh-net` | Đủ cho API | Chưa chạy với trang AppSample hoặc Kongying thật |
| Tải dữ liệu, HD tile, cập nhật app | unit, nhánh HD tile có trong replay | Một phần | Phần cần mạng thật chưa kiểm |
| Bản phát hành, bộ cài | `build_release.ps1`, `audit_release.py` | Đủ | Không cần thêm |
| GameEnv và Arena | chưa làm | Chưa | Chỉ làm khi có lỗi cần tới |

Muốn lên cấp 2, các dòng **Chưa** và **Unit** của tính năng hay dùng (toast và bíp, `Alt+F`, Nav HUD) phải lên
được **Đủ**.

Nhánh HD tile đã được replay đi qua: phiên `011805` ngày 2026-11-27 có 10 lần đọc đĩa, 235 lần `hd_sobel_fast`,
102 lần `hd_sift` và 0 lần tải mạng.

## Nhật ký thay đổi workflow

| Ngày | Thay đổi | Số liệu dẫn tới |
|---|---|---|
| 2026-11-27 | Tạo hồ sơ, đặt cấp 1 | Vụ mất bản quay gốc sau một câu "đồng ý dọn dẹp" chung chung |
| 2026-11-27 | Thêm bản đồ phủ test và các gate `web-bench`, `ext-smoke`, `ext-i18n`, `blackbox`. Lỗi tìm thấy chỉ ghi vào dev note kèm ca test, không sửa ngay | Người dùng thấy agent sửa hết tính năng này tới tính năng khác thay vì hoàn thành bộ test |
| 2026-11-27 | Bản quay ghi `provenance` (commit, nguồn dữ liệu, hash dữ liệu) và chép bộ dữ liệu app đang dùng; replay dùng bộ đó, `qa` thêm cột `Prov` (chỉ để xem) | Điều tra bước 3: `011544` lệch vì baseline live từ build cũ; replay đọc `data/` của repo trong khi app cài đặt đọc `%LOCALAPPDATA%`, dùng dữ liệu đã cài thì lệch trạng thái giảm 20–30 % |
| 2026-11-27 | Bộ ghi giữ mọi tick `Settling` của M-Map và đếm tick bị bỏ (cột `Unrec`); replay phát lại hotkey và dữ liệu web qua đúng hàm của app (cột `Nav`); thêm kịch bản `tests/scenarios` với `known-fail`/`xpass` không chặn qa | 32/42 lần đồng bộ lúc chơi thật nằm trên tick không được ghi; replay chưa phát hotkey nên Nav HUD chỉ có unit test; người dùng chọn lỗi đã biết báo `known-fail` thay vì làm qa đỏ |
| 2026-11-27 | Thêm lệnh kiểm duy nhất trung lập công cụ `scripts/check.ps1` và checklist quay phiên `docs/recording-checklist.md`; gia cố các test nhạy tải (`507/507` × 2 lượt ở 48 luồng); gộp trùng SHA-256 (`gh-net` dùng lại `gh-core`); sửa lỗi `Alt+F6` (`clear_user_locks()`) | Nhu cầu một lệnh kiểm dùng chung cho mọi agent; 3 nhóm test chập chờn khi chạy song song toàn workspace; lỗi `Alt+F6` không gỡ khóa `Alt+E` trong kịch bản `011805` |
| 2026-11-27 | Thêm cổng `frames` (`gh-sim frames`, ảnh tĩnh HUD 21:9/16:9 & M-Map map 40); mở rộng `ext-smoke` với `tests/extension_dom_fixtures.mjs` trên popup DOM EN/VI thật; thêm `UiFrameSnapshot` + 4 kịch bản `Alt+H/F7/X/`` ` trên `011805`; giới hạn kiểm tra `Prov` vào `ENGINE_PROVENANCE_PATHS` và sửa `build.rs` đọc runtime `CARGO_MANIFEST_DIR`; sửa `initial_state` + đồng hồ mô phỏng xác định cho nav replay và thêm kịch bản `210432` `known_bug`; thêm cột thông tin `Truth` trong `gh-sim qa` | Đóng các khoảng trống kiểm thử ngoại tuyến (bố cục 21:9, nút `"Mark Pin"` tiếng Anh, chuyển trạng thái UI/Radar, chứng cứ provenance, sai lệch `nav` không tất định và `carried_over`, bẫy false-lock/nhảy cóc M-Map trên map 40) mà không sửa logic CV/extension đang khóa |

