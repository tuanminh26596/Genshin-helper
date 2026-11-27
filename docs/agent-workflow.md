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

`replay-qa` chạy khoảng 5,5 phút cho 12 phiên (đo 318 s ngày 2026-11-27). Nên giữ nó làm gate cho các glob CV,
không chạy cho mọi thay đổi.

## Cửa một chiều cụ thể

| Đối tượng | Nhóm | Ràng buộc hiện tại | Lớp cứng nhất? |
|---|---|---|---|
| `scratch\qa_sessions\` (7 phiên, **là bản duy nhất**: `session_rec\` gốc đã bị xóa, thư mục này bị `.gitignore`, 4 phiên `libx265` đã mất màu so với bản gốc) | xóa dữ liệu | chỉ là lời dặn "tránh `git clean -fdx`" | **chưa**. Đề xuất: sao lưu sang ổ khác và đặt ACL chỉ cho đọc |
| `gh-sim qa --bless`, `blessed_poses.jsonl` | đổi đáp án đúng | mục `policy` trong `qa_baseline.json` | chưa. Đề xuất: gate so sha256 của baseline với một bản tham chiếu chỉ người được cập nhật |
| ngưỡng trong `tests/qa_baseline.json`, `tests/verified_gt.json` | nới ngưỡng, đổi đáp án đúng | mục `policy` | chưa. Đề xuất như trên |
| thêm `#[ignore]` hoặc skip test | nới ngưỡng | không có | chưa. Đề xuất: lint hoặc gate đếm số `#[ignore]` |
| GitHub Release, `SHA256SUMS.txt`, auto-updater | phát hành | thao tác tay | gần như đủ (người tự làm) |

## Ngoài phạm vi gate

Agent không được nói "đã kiểm" cho các vùng sau, vì chưa có gate phủ:

- Map Biển Trăng Dạt Dào và vùng tối Mặt Trăng không nhận diện được (người dùng báo ở bước 576).
- HUD báo nhận diện bị trễ và âm báo đến muộn sau khi bấm M.
- Replay hotkey khi tua nhanh (khoảng xác nhận 3 giây dùng `Instant::now`).
- GameEnv và Arena (chưa làm).

Muốn lên cấp 2 thì trước hết phải quay thêm phiên cho các ca này và đưa vào `qa`.

Nhánh HD tile đã ra khỏi danh sách này: replay phiên `011805` ngày 2026-11-27 đi qua nó với 10 lần đọc đĩa,
235 lần `hd_sobel_fast`, 102 lần `hd_sift` và 0 lần tải mạng.

## Nhật ký thay đổi workflow

| Ngày | Thay đổi | Số liệu dẫn tới |
|---|---|---|
| 2026-11-27 | Tạo hồ sơ, đặt cấp 1 | Vụ mất bản quay gốc sau một câu "đồng ý dọn dẹp" chung chung |

