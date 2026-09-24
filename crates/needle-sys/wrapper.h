/* bindgen entry point. `needle.h` is resolved by build.rs, which adds the
   chosen header directory to the include path: NEEDLE_LIB_DIR, else
   vendor/<target-triple>/, else this crate's own committed copy. */
#include "needle.h"
