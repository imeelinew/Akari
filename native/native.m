#import "native.h"

#import <AVFoundation/AVFoundation.h>
#import <CoreMedia/CoreMedia.h>
#import <Foundation/Foundation.h>

#include <math.h>
#include <stdatomic.h>
#include <stddef.h>

_Static_assert(sizeof(int) == 4, "AKSnapshot requires 32-bit C int");
_Static_assert(sizeof(AKSnapshot) == 56, "AKSnapshot Rust/C size mismatch");
_Static_assert(offsetof(AKSnapshot, generation) == 0, "generation offset");
_Static_assert(offsetof(AKSnapshot, position) == 8, "position offset");
_Static_assert(offsetof(AKSnapshot, duration) == 16, "duration offset");
_Static_assert(offsetof(AKSnapshot, volume) == 24, "volume offset");
_Static_assert(offsetof(AKSnapshot, status) == 32, "status offset");
_Static_assert(offsetof(AKSnapshot, paused) == 36, "paused offset");
_Static_assert(offsetof(AKSnapshot, buffering) == 40, "buffering offset");
_Static_assert(offsetof(AKSnapshot, ended) == 44, "ended offset");
_Static_assert(offsetof(AKSnapshot, spatial) == 48, "spatial offset");
_Static_assert(offsetof(AKSnapshot, error_code) == 52, "error_code offset");

static _Atomic(BOOL) akMainStopped = YES;
static _Atomic(BOOL) akMainAvailable = NO;

static BOOL AKSourceIsValid(NSString *source) {
    if (source.length == 0) {
        return NO;
    }
    if ([source hasPrefix:@"/"]) {
        return YES;
    }

    NSURL *url = [NSURL URLWithString:source];
    if (url == nil || url.host.length == 0) {
        return NO;
    }
    NSString *scheme = url.scheme.lowercaseString;
    return [scheme isEqualToString:@"http"] || [scheme isEqualToString:@"https"];
}

static double AKNonNegative(double value) {
    if (!isfinite(value)) {
        return 0.0;
    }
    return fmax(0.0, value);
}

@interface AKNativePlayer : NSObject {
    AVPlayer *_player;
    AVURLAsset *_asset;
    BOOL _spatial;
    BOOL _paused;
    BOOL _ready;
    double _initialPosition;
    double _seekTarget;
    _Atomic(uint64_t) _generation;
    _Atomic(uint64_t) _endedGeneration;
    id _endObserver;
}

- (instancetype)initWithSpatial:(BOOL)spatial;
- (void)execute:(void (^)(void))work;
- (void)shutdown;
- (void)load:(NSDictionary *)command;
- (AKSnapshot)takeSnapshot;

@end

@implementation AKNativePlayer

- (instancetype)initWithSpatial:(BOOL)spatial {
    if ((self = [super init])) {
        _spatial = spatial;
        _paused = YES;
        _seekTarget = NAN;
        atomic_init(&_generation, 0);
        atomic_init(&_endedGeneration, 0);
        _player = [AVPlayer new];
        _player.automaticallyWaitsToMinimizeStalling = YES;
        _player.volume = 0.7;
    }
    return self;
}

- (void)execute:(void (^)(void))work {
    if ([NSThread isMainThread] || !atomic_load(&akMainAvailable)) {
        @autoreleasepool {
            work();
        }
        return;
    }

    dispatch_sync(dispatch_get_main_queue(), ^{
        @autoreleasepool {
            work();
        }
    });
}

- (void)clearItem {
    if (_endObserver != nil) {
        [[NSNotificationCenter defaultCenter] removeObserver:_endObserver];
        _endObserver = nil;
    }
    [_player pause];
    [_player.currentItem cancelPendingSeeks];
    [_asset cancelLoading];
    [_player replaceCurrentItemWithPlayerItem:nil];
    _asset = nil;
    _ready = NO;
    _seekTarget = NAN;
    atomic_store(&_endedGeneration, 0);
}

- (void)load:(NSDictionary *)command {
    [self clearItem];

    if (_player.status == AVPlayerStatusFailed) {
        float volume = _player.volume;
        _player = [AVPlayer new];
        _player.automaticallyWaitsToMinimizeStalling = YES;
        _player.volume = volume;
    }

    uint64_t generation = [command[@"generation"] unsignedLongLongValue];
    atomic_store(&_generation, generation);
    _paused = [command[@"paused"] boolValue];
    _initialPosition = AKNonNegative([command[@"position"] doubleValue]);

    NSString *source = command[@"source"];
    NSURL *url = [source hasPrefix:@"/"]
        ? [NSURL fileURLWithPath:source]
        : [NSURL URLWithString:source];
    _asset = [AVURLAsset URLAssetWithURL:url options:nil];

    AVPlayerItem *item = [AVPlayerItem playerItemWithAsset:_asset];
    item.preferredForwardBufferDuration = 2;
    item.allowedAudioSpatializationFormats = _spatial
        ? AVAudioSpatializationFormatMonoStereoAndMultichannel
        : AVAudioSpatializationFormatNone;

    _endObserver = [[NSNotificationCenter defaultCenter]
        addObserverForName:AVPlayerItemDidPlayToEndTimeNotification
                    object:item
                     queue:nil
                usingBlock:^(__unused NSNotification *note) {
        if (atomic_load(&self->_generation) == generation) {
            atomic_store(&self->_endedGeneration, generation);
        }
    }];
    [_player replaceCurrentItemWithPlayerItem:item];
}

- (void)setPaused:(BOOL)paused {
    _paused = paused;
    if (paused) {
        [_player pause];
    } else if (_ready) {
        [_player play];
    }
}

- (void)setVolume:(double)volume {
    if (!isfinite(volume)) {
        return;
    }
    _player.volume = (float)(fmin(100.0, fmax(0.0, volume)) / 100.0);
}

- (void)setSpatial:(BOOL)enabled {
    _spatial = enabled;
    _player.currentItem.allowedAudioSpatializationFormats = enabled
        ? AVAudioSpatializationFormatMonoStereoAndMultichannel
        : AVAudioSpatializationFormatNone;
}

- (void)seek:(double)value absolute:(BOOL)absolute {
    if (_player.currentItem == nil || !isfinite(value)) {
        return;
    }

    double position = absolute
        ? value
        : CMTimeGetSeconds(_player.currentTime) + value;
    double duration = CMTimeGetSeconds(_player.currentItem.duration);
    if (isfinite(duration) && duration > 0) {
        position = fmin(position, duration);
    }
    position = AKNonNegative(position);

    _seekTarget = position;
    atomic_store(&_endedGeneration, 0);
    [_player seekToTime:CMTimeMakeWithSeconds(position, 600)
        toleranceBefore:kCMTimeZero
         toleranceAfter:kCMTimeZero];
}

- (AKSnapshot)takeSnapshot {
    AKSnapshot result = {0};
    result.generation = atomic_load(&_generation);
    result.paused = _paused;
    result.volume = round(_player.volume * 10000.0) / 100.0;
    result.spatial = _spatial;

    AVPlayerItem *item = _player.currentItem;
    if (item == nil) {
        return result;
    }

    NSError *error = item.status == AVPlayerItemStatusFailed ? item.error : _player.error;
    if (item.status == AVPlayerItemStatusFailed || _player.status == AVPlayerStatusFailed) {
        result.status = 3;
        result.error_code = (int)error.code;
        return result;
    }

    if (item.status != AVPlayerItemStatusReadyToPlay) {
        result.status = 1;
        result.buffering = !_paused;
        return result;
    }

    if (!_ready) {
        _ready = YES;
        for (AVPlayerItemTrack *track in item.tracks) {
            if ([track.assetTrack.mediaType isEqualToString:AVMediaTypeVideo]) {
                track.enabled = NO;
            }
        }
        if (_initialPosition > 0) {
            [self seek:_initialPosition absolute:YES];
        }
        if (!_paused) {
            [_player play];
        }
    }

    result.status = 2;
    result.spatial = item.allowedAudioSpatializationFormats
        == AVAudioSpatializationFormatMonoStereoAndMultichannel;

    double position = CMTimeGetSeconds(_player.currentTime);
    double duration = CMTimeGetSeconds(item.duration);
    position = AKNonNegative(position);
    if (isfinite(_seekTarget)) {
        if (fabs(position - _seekTarget) < 0.5) {
            _seekTarget = NAN;
        } else {
            position = _seekTarget;
        }
    }

    result.position = position;
    result.duration = isfinite(duration) ? fmax(0.0, duration) : 0.0;
    result.buffering = !_paused
        && _player.timeControlStatus == AVPlayerTimeControlStatusWaitingToPlayAtSpecifiedRate;
    result.ended = result.generation != 0
        && atomic_load(&_endedGeneration) == result.generation;
    return result;
}

- (void)stop:(uint64_t)generation {
    [self clearItem];
    atomic_store(&_generation, generation);
    _paused = YES;
}

- (void)shutdown {
    [self execute:^{
        [self clearItem];
        self->_player = nil;
    }];
}

@end

void ak_main_prepare(void) {
    atomic_store(&akMainStopped, NO);
    atomic_store(&akMainAvailable, YES);
}

void ak_main_run(void) {
    @autoreleasepool {
        CFRunLoopSourceContext context = {0};
        CFRunLoopSourceRef source = CFRunLoopSourceCreate(NULL, 0, &context);
        CFRunLoopAddSource(CFRunLoopGetMain(), source, kCFRunLoopDefaultMode);
        while (!atomic_load(&akMainStopped)) {
            @autoreleasepool {
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.25, true);
            }
        }
        CFRunLoopRemoveSource(CFRunLoopGetMain(), source, kCFRunLoopDefaultMode);
        CFRelease(source);
    }
    atomic_store(&akMainAvailable, NO);
}

void ak_main_stop(void) {
    atomic_store(&akMainStopped, YES);
    atomic_store(&akMainAvailable, NO);
    CFRunLoopWakeUp(CFRunLoopGetMain());
}

int ak_main_is_main_thread(void) {
    return [NSThread isMainThread] ? 1 : 0;
}

void *ak_player_create(int spatial) {
    @autoreleasepool {
        __block AKNativePlayer *player = nil;
        void (^create)(void) = ^{
            @autoreleasepool {
                player = [[AKNativePlayer alloc] initWithSpatial:spatial != 0];
            }
        };
        if ([NSThread isMainThread] || !atomic_load(&akMainAvailable)) {
            create();
        } else {
            dispatch_sync(dispatch_get_main_queue(), create);
        }
        return player == nil ? NULL : (__bridge_retained void *)player;
    }
}

int ak_player_load(
    void *handle,
    const char *source,
    int paused,
    double position,
    uint64_t generation
) {
    @autoreleasepool {
        if (handle == NULL || source == NULL) {
            return 0;
        }

        NSString *path = [NSString stringWithUTF8String:source];
        if (!AKSourceIsValid(path)) {
            return 0;
        }

        AKNativePlayer *player = (__bridge AKNativePlayer *)handle;
        NSDictionary *command = @{
            @"source": path,
            @"paused": @(paused != 0),
            @"position": @(AKNonNegative(position)),
            @"generation": @(generation)
        };
        [player execute:^{
            [player load:command];
        }];
        return 1;
    }
}

void ak_player_pause(void *handle, int paused) {
    @autoreleasepool {
        AKNativePlayer *player = (__bridge AKNativePlayer *)handle;
        if (player != nil) {
            [player execute:^{
                [player setPaused:paused != 0];
            }];
        }
    }
}

void ak_player_seek(void *handle, double seconds, int absolute) {
    @autoreleasepool {
        AKNativePlayer *player = (__bridge AKNativePlayer *)handle;
        if (player != nil) {
            [player execute:^{
                [player seek:seconds absolute:absolute != 0];
            }];
        }
    }
}

void ak_player_volume(void *handle, double volume) {
    @autoreleasepool {
        AKNativePlayer *player = (__bridge AKNativePlayer *)handle;
        if (player != nil) {
            [player execute:^{
                [player setVolume:volume];
            }];
        }
    }
}

void ak_player_spatial(void *handle, int enabled) {
    @autoreleasepool {
        AKNativePlayer *player = (__bridge AKNativePlayer *)handle;
        if (player != nil) {
            [player execute:^{
                [player setSpatial:enabled != 0];
            }];
        }
    }
}

void ak_player_stop(void *handle, uint64_t generation) {
    @autoreleasepool {
        AKNativePlayer *player = (__bridge AKNativePlayer *)handle;
        if (player != nil) {
            [player execute:^{
                [player stop:generation];
            }];
        }
    }
}

AKSnapshot ak_player_snapshot(void *handle) {
    @autoreleasepool {
        AKSnapshot result = {0};
        result.paused = 1;
        AKNativePlayer *player = (__bridge AKNativePlayer *)handle;
        if (player != nil) {
            __block AKSnapshot state = {0};
            [player execute:^{
                state = [player takeSnapshot];
            }];
            result = state;
        }
        return result;
    }
}

void ak_player_destroy(void *handle) {
    @autoreleasepool {
        if (handle == NULL) {
            return;
        }
        AKNativePlayer *player = (__bridge_transfer AKNativePlayer *)handle;
        [player shutdown];
    }
}
