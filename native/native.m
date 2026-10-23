#import "native.h"

#import <Accelerate/Accelerate.h>
#import <AVFoundation/AVFoundation.h>
#import <AudioToolbox/AudioToolbox.h>
#import <CoreMedia/CoreMedia.h>
#import <Foundation/Foundation.h>
#import <MediaToolbox/MediaToolbox.h>

#include <math.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stddef.h>
#include <string.h>

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

#define AK_FFT_SIZE 2048
#define AK_FFT_LOG2 11
#define AK_SPECTRUM_BANDS 32

typedef struct {
    AudioStreamBasicDescription format;
    FFTSetup fft;
    float samples[AK_FFT_SIZE];
    float window[AK_FFT_SIZE];
    float work[AK_FFT_SIZE];
    float real[AK_FFT_SIZE / 2];
    float imag[AK_FFT_SIZE / 2];
    float power[AK_FFT_SIZE / 2];
    float levels[AK_SPECTRUM_BANDS];
    _Atomic(uint32_t) sampleCount;
    _Atomic(uint64_t) epoch;
    pthread_mutex_t lock;
} AKSpectrum;

static uint16_t AKReadU16(const uint8_t *bytes, BOOL bigEndian) {
    if (bigEndian) {
        return (uint16_t)(((uint16_t)bytes[0] << 8) | bytes[1]);
    }
    return (uint16_t)(bytes[0] | ((uint16_t)bytes[1] << 8));
}

static uint32_t AKReadU32(const uint8_t *bytes, BOOL bigEndian) {
    if (bigEndian) {
        return ((uint32_t)bytes[0] << 24)
            | ((uint32_t)bytes[1] << 16)
            | ((uint32_t)bytes[2] << 8)
            | bytes[3];
    }
    return bytes[0]
        | ((uint32_t)bytes[1] << 8)
        | ((uint32_t)bytes[2] << 16)
        | ((uint32_t)bytes[3] << 24);
}

static uint64_t AKReadU64(const uint8_t *bytes, BOOL bigEndian) {
    uint64_t value = 0;
    if (bigEndian) {
        for (NSUInteger index = 0; index < sizeof(value); index++) {
            value = (value << 8) | bytes[index];
        }
    } else {
        for (NSUInteger index = 0; index < sizeof(value); index++) {
            value |= (uint64_t)bytes[index] << (index * 8);
        }
    }
    return value;
}

static BOOL AKReadNormalizedSample(
    const uint8_t *bytes,
    const AudioStreamBasicDescription *format,
    float *result
) {
    BOOL bigEndian = (format->mFormatFlags & kAudioFormatFlagIsBigEndian) != 0;
    if (format->mFormatFlags & kAudioFormatFlagIsFloat) {
        if (format->mBitsPerChannel == 32) {
            uint32_t bits = AKReadU32(bytes, bigEndian);
            float value;
            memcpy(&value, &bits, sizeof(value));
            if (!isfinite(value)) {
                return NO;
            }
            *result = fminf(1.0f, fmaxf(-1.0f, value));
            return YES;
        }
        if (format->mBitsPerChannel == 64) {
            uint64_t bits = AKReadU64(bytes, bigEndian);
            double value;
            memcpy(&value, &bits, sizeof(value));
            if (!isfinite(value)) {
                return NO;
            }
            *result = (float)fmin(1.0, fmax(-1.0, value));
            return YES;
        }
        return NO;
    }

    BOOL isSigned = (format->mFormatFlags & kAudioFormatFlagIsSignedInteger) != 0;
    switch (format->mBitsPerChannel) {
        case 8:
            *result = isSigned
                ? (float)((int8_t)bytes[0]) / 128.0f
                : ((float)bytes[0] - 128.0f) / 128.0f;
            return YES;
        case 16: {
            uint16_t bits = AKReadU16(bytes, bigEndian);
            if (isSigned) {
                int16_t value;
                memcpy(&value, &bits, sizeof(value));
                *result = (float)value / 32768.0f;
            } else {
                *result = ((float)bits - 32768.0f) / 32768.0f;
            }
            return YES;
        }
        case 24: {
            uint32_t bits;
            if (bigEndian) {
                bits = ((uint32_t)bytes[0] << 16)
                    | ((uint32_t)bytes[1] << 8)
                    | bytes[2];
            } else {
                bits = bytes[0]
                    | ((uint32_t)bytes[1] << 8)
                    | ((uint32_t)bytes[2] << 16);
            }
            if (isSigned && (bits & 0x800000U)) {
                bits |= 0xff000000U;
            }
            int32_t value;
            memcpy(&value, &bits, sizeof(value));
            *result = isSigned
                ? (float)value / 8388608.0f
                : ((float)bits - 8388608.0f) / 8388608.0f;
            return YES;
        }
        case 32: {
            uint32_t bits = AKReadU32(bytes, bigEndian);
            if (isSigned) {
                int32_t value;
                memcpy(&value, &bits, sizeof(value));
                *result = (float)((double)value / 2147483648.0);
            } else {
                *result = (float)(((double)bits - 2147483648.0) / 2147483648.0);
            }
            return YES;
        }
        default:
            return NO;
    }
}

static void AKClearLevels(AKSpectrum *spectrum, BOOL blocking) {
    if (spectrum == NULL) {
        return;
    }
    atomic_fetch_add_explicit(&spectrum->epoch, 1, memory_order_acq_rel);
    atomic_store_explicit(&spectrum->sampleCount, 0, memory_order_release);
    int locked = blocking
        ? pthread_mutex_lock(&spectrum->lock)
        : pthread_mutex_trylock(&spectrum->lock);
    if (locked == 0) {
        memset(spectrum->levels, 0, sizeof(spectrum->levels));
        pthread_mutex_unlock(&spectrum->lock);
    }
}

static void AKComputeSpectrum(AKSpectrum *spectrum, uint64_t epoch) {
    if (atomic_load_explicit(&spectrum->epoch, memory_order_acquire) != epoch
        || spectrum->format.mSampleRate < 80.0
        || spectrum->format.mSampleRate > 768000.0) {
        return;
    }

    vDSP_vmul(spectrum->samples, 1, spectrum->window, 1, spectrum->work, 1, AK_FFT_SIZE);
    DSPSplitComplex split = { spectrum->real, spectrum->imag };
    vDSP_ctoz((const DSPComplex *)spectrum->work, 2, &split, 1, AK_FFT_SIZE / 2);
    vDSP_fft_zrip(spectrum->fft, &split, 1, AK_FFT_LOG2, FFT_FORWARD);
    spectrum->imag[0] = 0.0f;
    vDSP_zvmags(&split, 1, spectrum->power, 1, AK_FFT_SIZE / 2);

    float levels[AK_SPECTRUM_BANDS];
    double nyquist = spectrum->format.mSampleRate * 0.5;
    double upper = fmin(16000.0, nyquist);
    if (upper <= 40.0) {
        return;
    }
    for (int band = 0; band < AK_SPECTRUM_BANDS; band++) {
        double lowerFrequency =
            40.0 * pow(upper / 40.0, (double)band / AK_SPECTRUM_BANDS);
        double upperFrequency =
            40.0 * pow(upper / 40.0, (double)(band + 1) / AK_SPECTRUM_BANDS);
        int lower = (int)fmax(
            1.0,
            floor(lowerFrequency * AK_FFT_SIZE / spectrum->format.mSampleRate)
        );
        int high = (int)fmin(
            AK_FFT_SIZE / 2,
            fmax(lower + 1.0, ceil(upperFrequency * AK_FFT_SIZE / spectrum->format.mSampleRate))
        );
        float peak = 0.0f;
        for (int bin = lower; bin < high; bin++) {
            peak = fmaxf(peak, spectrum->power[bin]);
        }
        float decibels = 10.0f * log10f(fmaxf(
            peak / (float)(AK_FFT_SIZE * AK_FFT_SIZE),
            1.0e-12f
        ));
        levels[band] = fminf(1.0f, fmaxf(0.0f, (decibels + 75.0f) / 65.0f));
    }

    if (pthread_mutex_trylock(&spectrum->lock) == 0) {
        if (atomic_load_explicit(&spectrum->epoch, memory_order_acquire) == epoch) {
            memcpy(spectrum->levels, levels, sizeof(levels));
        }
        pthread_mutex_unlock(&spectrum->lock);
    }
}

static BOOL AKAppendSample(AKSpectrum *spectrum, float sample, uint64_t epoch) {
    uint32_t count = atomic_load_explicit(&spectrum->sampleCount, memory_order_acquire);
    if (count >= AK_FFT_SIZE
        || atomic_load_explicit(&spectrum->epoch, memory_order_acquire) != epoch) {
        return NO;
    }
    spectrum->samples[count] = sample;
    uint32_t expected = count;
    if (!atomic_compare_exchange_strong_explicit(
            &spectrum->sampleCount,
            &expected,
            count + 1,
            memory_order_release,
            memory_order_acquire)) {
        return NO;
    }
    if (count + 1 == AK_FFT_SIZE) {
        atomic_store_explicit(&spectrum->sampleCount, 0, memory_order_release);
        AKComputeSpectrum(spectrum, epoch);
        return NO;
    }
    return YES;
}

static void AKTapInit(MTAudioProcessingTapRef tap, void *info, void **storage) {
    (void)tap;
    (void)info;
    if (storage == NULL) {
        return;
    }
    *storage = NULL;

    AKSpectrum *spectrum = calloc(1, sizeof(*spectrum));
    if (spectrum == NULL) {
        return;
    }
    if (pthread_mutex_init(&spectrum->lock, NULL) != 0) {
        free(spectrum);
        return;
    }
    atomic_init(&spectrum->sampleCount, 0);
    atomic_init(&spectrum->epoch, 0);
    spectrum->fft = vDSP_create_fftsetup(AK_FFT_LOG2, kFFTRadix2);
    if (spectrum->fft == NULL) {
        pthread_mutex_destroy(&spectrum->lock);
        free(spectrum);
        return;
    }
    vDSP_hann_window(spectrum->window, AK_FFT_SIZE, vDSP_HANN_NORM);
    *storage = spectrum;
}

static void AKTapFinalize(MTAudioProcessingTapRef tap) {
    AKSpectrum *spectrum = MTAudioProcessingTapGetStorage(tap);
    if (spectrum == NULL) {
        return;
    }
    if (spectrum->fft != NULL) {
        vDSP_destroy_fftsetup(spectrum->fft);
    }
    pthread_mutex_destroy(&spectrum->lock);
    free(spectrum);
}

static void AKTapPrepare(
    MTAudioProcessingTapRef tap,
    CMItemCount maxFrames,
    const AudioStreamBasicDescription *format
) {
    (void)maxFrames;
    AKSpectrum *spectrum = MTAudioProcessingTapGetStorage(tap);
    if (spectrum != NULL && format != NULL) {
        spectrum->format = *format;
        AKClearLevels(spectrum, NO);
    }
}

static void AKTapUnprepare(MTAudioProcessingTapRef tap) {
    AKClearLevels(MTAudioProcessingTapGetStorage(tap), NO);
}

static void AKTapProcess(
    MTAudioProcessingTapRef tap,
    CMItemCount requestedFrames,
    __unused MTAudioProcessingTapFlags flags,
    AudioBufferList *buffers,
    CMItemCount *outFrames,
    MTAudioProcessingTapFlags *outFlags
) {
    if (outFrames == NULL || outFlags == NULL
        || MTAudioProcessingTapGetSourceAudio(
            tap,
            requestedFrames,
            buffers,
            outFlags,
            NULL,
            outFrames
        ) != noErr) {
        return;
    }

    AKSpectrum *spectrum = MTAudioProcessingTapGetStorage(tap);
    if (spectrum == NULL || spectrum->fft == NULL
        || buffers == NULL || *outFrames <= 0
        || spectrum->format.mFormatID != kAudioFormatLinearPCM
        || spectrum->format.mSampleRate <= 0.0
        || spectrum->format.mChannelsPerFrame == 0
        || spectrum->format.mBitsPerChannel == 0
        || spectrum->format.mBytesPerFrame == 0) {
        return;
    }

    if ((*outFlags & kMTAudioProcessingTapFlag_StartOfStream) != 0) {
        AKClearLevels(spectrum, NO);
    }
    uint64_t epoch = atomic_load_explicit(&spectrum->epoch, memory_order_acquire);

    BOOL floatFormat =
        (spectrum->format.mFormatFlags & kAudioFormatFlagIsFloat) != 0;
    if ((!floatFormat
            && (spectrum->format.mBitsPerChannel != 8)
            && (spectrum->format.mBitsPerChannel != 16)
            && (spectrum->format.mBitsPerChannel != 24)
            && (spectrum->format.mBitsPerChannel != 32))
        || (floatFormat
            && spectrum->format.mBitsPerChannel != 32
            && spectrum->format.mBitsPerChannel != 64)) {
        return;
    }

    uint32_t sampleBytes = (spectrum->format.mBitsPerChannel + 7) / 8;
    BOOL nonInterleaved =
        (spectrum->format.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0;
    // mBytesPerFrame describes one buffer's frame. For planar PCM each
    // buffer contains ONE channel, even though mChannelsPerFrame is stereo
    // (or multichannel). Dividing its stride by the total channel count would
    // reject the normal AVFoundation planar Float32 format as undersized.
    uint32_t frameStride = spectrum->format.mBytesPerFrame;
    uint32_t channelStride = nonInterleaved
        ? frameStride
        : frameStride / spectrum->format.mChannelsPerFrame;
    if (channelStride < sampleBytes) {
        return;
    }

    for (CMItemCount frame = 0; frame < *outFrames; frame++) {
        double sum = 0.0;
        uint32_t channelCount = 0;
        for (UInt32 bufferIndex = 0; bufferIndex < buffers->mNumberBuffers; bufferIndex++) {
            const AudioBuffer *buffer = &buffers->mBuffers[bufferIndex];
            if (buffer->mData == NULL) {
                continue;
            }
            uint32_t channels = buffer->mNumberChannels;
            if (channels == 0 && !nonInterleaved) {
                channels = spectrum->format.mChannelsPerFrame;
            }
            if (channels == 0 || (nonInterleaved && channels != 1)) {
                continue;
            }
            size_t frameOffset = (size_t)frame * frameStride;
            size_t required = frameOffset + (size_t)(channels - 1) * channelStride + sampleBytes;
            if (required > buffer->mDataByteSize) {
                continue;
            }
            const uint8_t *data = buffer->mData;
            for (uint32_t channel = 0; channel < channels; channel++) {
                float value;
                size_t offset = frameOffset + (size_t)channel * channelStride;
                if (AKReadNormalizedSample(data + offset, &spectrum->format, &value)) {
                    sum += value;
                    channelCount++;
                }
            }
        }
        float sample = channelCount == 0 ? 0.0f : (float)(sum / channelCount);
        if (!AKAppendSample(spectrum, sample, epoch)) {
            break;
        }
    }
}

@interface AKNativePlayer : NSObject {
    AVPlayer *_player;
    MTAudioProcessingTapRef _tap;
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
- (void)clearSpectrum;
- (AKSnapshot)takeSnapshot;
- (void)copySpectrum:(float *)levels;

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

- (void)clearSpectrum {
    if (_tap == NULL) {
        return;
    }
    AKClearLevels(MTAudioProcessingTapGetStorage(_tap), YES);
}

- (void)clearItem {
    if (_endObserver != nil) {
        [[NSNotificationCenter defaultCenter] removeObserver:_endObserver];
        _endObserver = nil;
    }
    [self clearSpectrum];
    [_player pause];
    [_player.currentItem cancelPendingSeeks];
    [_asset cancelLoading];
    [_player replaceCurrentItemWithPlayerItem:nil];
    if (_tap != NULL) {
        CFRelease(_tap);
        _tap = NULL;
    }
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
        [self clearSpectrum];
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

    [self clearSpectrum];
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
        for (AVPlayerItemTrack *track in item.tracks) {
            if (![track.assetTrack.mediaType isEqualToString:AVMediaTypeAudio]) {
                continue;
            }
            MTAudioProcessingTapCallbacks callbacks;
            memset(&callbacks, 0, sizeof(callbacks));
            callbacks.version = kMTAudioProcessingTapCallbacksVersion_0;
            callbacks.init = AKTapInit;
            callbacks.finalize = AKTapFinalize;
            callbacks.prepare = AKTapPrepare;
            callbacks.unprepare = AKTapUnprepare;
            callbacks.process = AKTapProcess;
            if (MTAudioProcessingTapCreate(
                    kCFAllocatorDefault,
                    &callbacks,
                    kMTAudioProcessingTapCreationFlag_PreEffects,
                    &_tap
                ) == noErr) {
                AVMutableAudioMixInputParameters *parameters =
                    [AVMutableAudioMixInputParameters
                        audioMixInputParametersWithTrack:track.assetTrack];
                parameters.audioTapProcessor = _tap;
                AVMutableAudioMix *audioMix = [AVMutableAudioMix audioMix];
                audioMix.inputParameters = @[parameters];
                item.audioMix = audioMix;
            }
            break;
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

- (void)copySpectrum:(float *)levels {
    if (levels == NULL) {
        return;
    }
    memset(levels, 0, AK_SPECTRUM_BANDS * sizeof(float));
    if (_tap == NULL || _paused
        || _player.timeControlStatus != AVPlayerTimeControlStatusPlaying) {
        return;
    }
    AKSpectrum *spectrum = MTAudioProcessingTapGetStorage(_tap);
    if (spectrum == NULL) {
        return;
    }
    if (pthread_mutex_trylock(&spectrum->lock) == 0) {
        memcpy(levels, spectrum->levels, AK_SPECTRUM_BANDS * sizeof(float));
        pthread_mutex_unlock(&spectrum->lock);
    }
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

void ak_player_spectrum(void *handle, float *levels) {
    @autoreleasepool {
        if (handle == NULL || levels == NULL) {
            return;
        }
        AKNativePlayer *player = (__bridge AKNativePlayer *)handle;
        [player execute:^{
            [player copySpectrum:levels];
        }];
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
