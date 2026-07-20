// Minimal bindgen input for the RGA (Rockchip 2D accelerator) FFI slice
// used by rga_transfer.rs.
//
// Deliberately does NOT `#include <im2d.h>` / <im2d_single.h> /
// <im2d_buffer.h> directly — those headers mix in C++-only syntax
// (default arguments on the `IM_API` overloads used by C++ callers)
// that bindgen/clang can't parse in C mode. `im2d_type.h` and `rga.h`
// are pure C (verified: no `::`, templates, classes, or default args)
// and hold everything we actually need: the `rga_buffer_t` struct
// layout, the `IM_STATUS` enum, and the `RK_FORMAT_*` constants. The
// handful of plain C-ABI (`_t`-suffixed) functions we call are
// re-declared below with their exact real signatures (confirmed
// against the installed librga-dev 2.2.0 headers on bilby-pir6s)
// rather than pulled in transitively.
#include <im2d_type.h>
#include <rga.h>

rga_buffer_t wrapbuffer_fd_t(int fd, int width, int height, int wstride, int hstride, int format);
rga_buffer_t wrapbuffer_virtualaddr_t(void *vir_addr, int width, int height, int wstride, int hstride, int format);
IM_STATUS imcopy_t(const rga_buffer_t src, rga_buffer_t dst, int sync);
IM_STATUS imcvtcolor_t(rga_buffer_t src, rga_buffer_t dst, int sfmt, int dfmt, int mode, int sync);
const char *imStrError_t(IM_STATUS status);
