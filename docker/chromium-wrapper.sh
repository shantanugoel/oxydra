#!/bin/sh
# Chromium stability wrapper for containerised/headless environments.
#
# Pinchtab reads CHROME_BINARY to determine the Chrome executable.  Pointing
# it here guarantees that the essential container-stability flags are always
# forwarded to Chromium regardless of how Pinchtab internally handles the
# separate CHROME_FLAGS environment variable (whose handling has had bugs in
# some Pinchtab versions).
#
# Flags applied:
#   --no-sandbox              Required: no user-namespace support in containers.
#   --disable-dev-shm-usage   Required: /dev/shm is typically only 64 MB in
#                             Docker; Chrome uses it for IPC shared memory and
#                             crashes without this flag on the default size.
#   --disable-gpu             Headless containers have no GPU; avoids GPU-
#                             related crashes.
#   --disable-software-rasterizer
#                             Disables the software rasterizer that Chrome
#                             falls back to when no GPU is available — it is
#                             slow and can OOM-crash on constrained devices
#                             such as Raspberry Pi.
exec /usr/bin/chromium \
    --no-sandbox \
    --disable-dev-shm-usage \
    --disable-gpu \
    --disable-software-rasterizer \
    "$@"
