# Checklist quay phiên test

Mỗi phiên quay là dữ liệu thật để bộ test (`gh-sim`) phát lại qua đúng code của app. Một số phần của bộ test
chỉ có số khi có phiên quay mới, vì 12 phiên hiện có được quay trước khi các phần đó tồn tại:

| Cột `qa` | Đo gì | Phiên cũ |
|---|---|---|
| `InGame` | Mỗi lần web xác nhận đồng bộ M-Map, game có hiện toast và bíp không | `n/a` |
| `Prov` | Bản quay dùng commit nào và bộ dữ liệu nào, so với lúc replay | `n/a` |
| `Unrec` | Số tick app chạy mà bộ ghi không giữ (`set` phải là 0) | `n/a` |
| `Nav` | Hotkey và dữ liệu web được phát lại; trạng thái nav replay lệch bao nhiêu frame so với lúc chơi | `n/a` (trừ 2 phiên có vài hotkey) |

## Trước khi quay

1. Tắt GenshinHelper nếu đang chạy, rồi build bản mới: `.\installer\build_release.ps1`.
   Script quay dùng `dist\GenshinHelper\GenshinHelper.exe`, nên bản build phải có các thay đổi mới nhất.
2. Mở bản đồ HoYoLAB trong trình duyệt có extension, đăng nhập và bật bộ lọc pin như khi chơi. Nếu không mở thì
   app không nhận được pin, và cột `Nav` chỉ có hotkey.
3. Chạy `scripts\record_session.bat`. App mở ở chế độ chờ. Trong game bấm `Alt+F8` để bắt đầu quay, bấm lần nữa
   để dừng. Mỗi lần dừng tạo một thư mục `sess_<ngày>_<giờ>` trong `%APPDATA%\GenshinHelper\session_rec`.
4. Bản quay là video nén không mất dữ liệu (lossless), nên rất nặng: 5 phiên ngày 2026-11-27, mỗi phiên 1–3 phút, tổng cộng
   13,7 GB. Kiểm tra ổ đĩa còn trống trước khi quay nhiều phiên.

## Trong lúc quay

Mỗi phiên nên dài 1–3 phút và có một mục tiêu rõ ràng. Nhớ thứ tự mình đã làm để ghi lại sau.

| Mục tiêu | Làm gì | Cột hoặc lỗi được đo |
|---|---|---|
| M-Map và toast | Bấm `M` trên map thường, rồi trên một sub-map (ví dụ Sương Nguyệt, map 40). Làm một lượt khi tab HoYoLAB đang hiện ở màn hình 2, một lượt khi tab bị thu nhỏ | `InGame`, `HUD`, `MMap`, `Unrec` |
| Hotkey điều hướng | Giữ tab HoYoLAB mở, đi về phía một nhóm pin. Bấm `Alt+E` vài lần, rồi `Alt+F6`, `Alt+H`, `Alt+X`, `Alt+F7`, `` Alt+` `` | `Nav`; lỗi `Alt+F6` không gỡ khóa của `Alt+E` |
| Mũi tên và nón camera | Đứng yên, xoay camera 360° chậm rồi nhanh; sau đó chạy vòng tròn | Số lần mũi tên/nón giật; lỗi mũi tên bám theo nón; nón giật ở vài góc |
| Vùng còn thiếu | Vùng tối Mặt Trăng, Biển Trăng Dạt Dào, hầm có đổi tầng | Theo dõi vị trí, map, tầng hầm |

`Alt+F` đánh dấu pin là thật trên tài khoản HoYoLAB. Chỉ bấm khi chấp nhận điều đó; `Alt+T` hoàn tác.

`Prov` và `Unrec` tự có trong mọi phiên quay mới, không cần làm gì thêm.

## Sau khi quay

1. Ghi lại cho agent: tên phiên, mục tiêu, và những gì đã thấy bất thường lúc chơi (ví dụ "bấm M ở map 40 không
   có bíp", "mũi tên giật khi xoay camera").
2. Không xóa bản quay gốc trong `session_rec`. Bản nén có thể mất màu hoặc chi tiết, nên bản gốc là bản duy nhất
   đáng tin để điều tra lại.
3. Agent phát lại phiên mới bằng `gh-sim` để đọc các cột ở trên và so với điều bạn ghi lại.
4. Chỉ khi bạn đồng ý, agent mới nén phiên vào `scratch\qa_sessions` (`gh-sim compact`) và bless nó. Từ lúc đó,
   phiên mới trở thành một phần của gate `qa`.
