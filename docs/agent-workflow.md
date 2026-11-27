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

| Tên | Lệnh | Phạm vi | `crew_gates`? |
|---|---|---|---|
| clippy | `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings` | `rust/**` | chưa |
| nextest | `cargo nextest run --manifest-path rust/Cargo.toml --workspace` | `rust/**` | chưa |
| replay-qa | `gh-sim qa --sessions scratch\qa_sessions --data-dir data --ffmpeg <ABS>` (12/12 ĐẠT, exit 0; gate cả độ giật mũi tên/nón từ 2026-11-27) | `rust/crates/gh-cv/**`, `rust/crates/gh-app/**`, `rust/crates/gh-sim/**` | chưa |
| e6 (đang `#[ignore]`) | lệnh chạy e6 kèm `--run-ignored`, cần xác nhận lại | `rust/crates/gh-cv/**` | chưa |
| web-bench | `pwsh -File scripts\web_bench.ps1 -Scenario scratch\web_scenario_all.json` (thêm `-Minimize` để mô phỏng tab bị ẩn sau game); cần tắt GenshinHelper vì dùng cổng 8765 | `extension/**`, `rust/crates/gh-net/**`, `rust/crates/gh-app/src/{tracking_loop,callbacks}.rs` | chưa |
| ext-smoke | `node tests/extension_browser_smoke.mjs` (Chromium headless, mô phỏng app, khoảng 7 s) | `extension/**` | chưa |
| ext-i18n | `.\.venv\Scripts\python.exe tests/verify_i18n_and_ext_v5.py` | `extension/**` | chưa |
| blackbox | `.\.venv\Scripts\python.exe tests/test_blackbox_contracts.py --server-exe <exe> --host-exe <exe>` (80 test, 4 tầng: route, biên, phối hợp nhiều tính năng, kịch bản) | `rust/crates/gh-net/**`, `rust/crates/gh-app/**` | chưa |

`replay-qa` chạy khoảng 5,5 phút cho 12 phiên (đo 318 s ngày 2026-11-27). Nên giữ nó làm gate cho các glob CV,
không chạy cho mọi thay đổi. Thứ tự chạy theo "Test ladder" trong `CLAUDE.md`.

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
| Theo dõi minimap: vị trí, map | `qa` (12 phiên, gate từng frame), unit `gh-cv` | Đủ, có ngoại lệ | Replay lệch so với lúc chơi thật ở `082307`, `011544`, `082625`. Chưa có phiên cho vùng tối Mặt Trăng và Biển Trăng Dạt Dào. Hầu hết các phiên ở trên mặt đất |
| Hướng mũi tên, nón camera | `qa` (gate số lần giật so với baseline) | Đủ để đo | Lỗi đã biết: mũi tên bám theo nón. Chưa đo độ mượt của nón |
| Hầm, nhiều tầng | `qa` (phiên có map 9) | Một phần | Chưa có phiên nào đổi tầng trong hầm |
| M-Map: nhận diện, đồng bộ, khung nhìn | `qa` (số lần sync, số frame `mmap_view_acquired`) | Đủ cho kết quả | Độ trễ lúc chơi thật chỉ có khi quay phiên mới (cột `HUD`) |
| Chỉ đường trên M-Map | `qa` (chỉ đếm `mmap_view`) | Một phần | Chưa kiểm hình chỉ đường vẽ ra |
| Đồng bộ vị trí lên web | `web-bench` (100 sự kiện, tab hiện và tab ẩn) | Đủ | Chưa đo trường hợp tab bị ẩn lâu hơn 5 phút (Chrome hãm hẹn giờ mạnh hơn) |
| Toast và bíp trong game | unit: luật quyết định (bắt được lỗi màn hình 2 khi gài lỗi), và mỗi `web_feedback` đều có một `hud_feedback` ghi game đã hiện gì. Cột `InGame` của `qa` kiểm trên bản quay thật | Unit; đo được trên bản quay mới | 12 phiên hiện có chưa có sự kiện `hud_feedback`, nên `InGame` là `n/a`. Cần quay phiên mới bằng bản build có thay đổi này. `web-bench` dùng callback giả, chưa chạy qua `on_action_feedback` thật. `hud_feedback` ghi việc app đã gửi toast và xếp hàng tiếng bíp, không phải pixel hay âm thanh đã ra thật |
| `Alt+F` đánh dấu pin | `blackbox` (feedback, rollback), unit | Unit | Chưa có bench đánh dấu thật trên web. Có lỗi đã biết "Web map could not mark it" |
| Nav HUD và các hotkey (`Alt+E/F6/T/H/F7/X`) | unit `gh-core` (`nav_controller`), `blackbox` (`nav_command`) | Unit | Replay chưa phát lại hotkey (bước 4). Có lỗi đã biết: `Alt+F6` không gỡ khóa của `Alt+E` |
| Radar (`` Alt+` ``) | compositor headless trong `gh-sim`, unit `gh-hud` | Một phần | Chưa so hình radar với ảnh chuẩn |
| Chuyển cửa sổ `Alt+Q`, chặn phím | unit `gh-win32` | Unit | Cần game thật, ngoài tầm replay |
| Chụp màn hình (GDI, WGC, HDR) | unit | Unit | Replay đi vòng qua phần chụp. Cần chơi thật |
| Bộ ghi phiên | unit `gh-app` | Unit | Không cần thêm |
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

