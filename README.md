```powershell
cargo build --release
```

```bash
clang --target=aarch64-linux-gnu -Oz -fno-unwind-tables -fno-asynchronous-unwind-tables -ffunction-sections -fdata-sections -Wl,--gc-sections -Wl,--strip-all -Wall -Wextra -Werror -pthread pencast-work.c -o pencast-work -ldl && readelf --version-info pencast-work | grep -oE 'GLIBC_[0-9.]+' | sed 's/GLIBC_//' | sort -Vu | tail -n 1 | awk -F. '$1 < 2 || ($1 == 2 && $2 <= 36) { exit } { exit 1 }'
```
