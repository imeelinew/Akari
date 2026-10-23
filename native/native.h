#ifndef AKARI_NATIVE_H
#define AKARI_NATIVE_H

#include <stdint.h>

typedef struct {
    uint64_t generation;
    double position;
    double duration;
    double volume;
    int status;
    int paused;
    int buffering;
    int ended;
    int spatial;
    int error_code;
} AKSnapshot;

void ak_main_prepare(void);
void ak_main_run(void);
void ak_main_stop(void);
int ak_main_is_main_thread(void);

void *ak_player_create(int spatial);
int ak_player_load(
    void *handle,
    const char *source,
    int paused,
    double position,
    uint64_t generation
);
void ak_player_pause(void *handle, int paused);
void ak_player_seek(void *handle, double seconds, int absolute);
void ak_player_volume(void *handle, double volume);
void ak_player_spatial(void *handle, int enabled);
void ak_player_stop(void *handle, uint64_t generation);
AKSnapshot ak_player_snapshot(void *handle);
void ak_player_spectrum(void *handle, float *levels);
void ak_player_destroy(void *handle);

#endif
