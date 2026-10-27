#pragma once
#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>
// Returns errno (zero on success). Buffers remain owned by FSKit.
int obd_open(const char *directory, bool writable, uint64_t *handle, uint64_t *capacity);
int obd_read(uint64_t handle, uint64_t offset, void *buffer, size_t length, size_t *actual);
int obd_write(uint64_t handle, uint64_t offset, const void *buffer, size_t length, size_t *actual);
int obd_flush(uint64_t handle);
int obd_close(uint64_t handle);
