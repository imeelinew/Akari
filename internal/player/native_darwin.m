#import "native.h"
#import <AVFoundation/AVFoundation.h>
#import <Foundation/Foundation.h>
#import <CoreMedia/CoreMedia.h>
#import <stdatomic.h>
#import <math.h>
#import <MediaToolbox/MediaToolbox.h>
#import <Accelerate/Accelerate.h>
#import <pthread.h>

#define AK_FFT 2048
#define AK_BANDS 32
typedef struct {
 AudioStreamBasicDescription format;
 FFTSetup fft;
 float samples[AK_FFT], window[AK_FFT], work[AK_FFT];
 float real[AK_FFT/2], imag[AK_FFT/2], power[AK_FFT/2];
 float levels[AK_BANDS];
 int count;
 pthread_mutex_t lock;
} AKSpectrum;
static void akTapInit(MTAudioProcessingTapRef tap, void *info, void **storage) {
 AKSpectrum *s=calloc(1,sizeof(*s));
 if(s){pthread_mutex_init(&s->lock,NULL);s->fft=vDSP_create_fftsetup(11,kFFTRadix2);vDSP_hann_window(s->window,AK_FFT,vDSP_HANN_NORM);}
 *storage=s;
}
static void akTapFinalize(MTAudioProcessingTapRef tap) {
 AKSpectrum *s=MTAudioProcessingTapGetStorage(tap);if(!s)return;
 if(s->fft)vDSP_destroy_fftsetup(s->fft);pthread_mutex_destroy(&s->lock);free(s);
}
static void akTapPrepare(MTAudioProcessingTapRef tap, CMItemCount frames, const AudioStreamBasicDescription *format) {
 AKSpectrum *s=MTAudioProcessingTapGetStorage(tap);if(s){s->format=*format;s->count=0;}
}
static void akTapUnprepare(MTAudioProcessingTapRef tap) {}
static void akTapProcess(MTAudioProcessingTapRef tap, CMItemCount frames, MTAudioProcessingTapFlags flags, AudioBufferList *buffers, CMItemCount *outFrames, MTAudioProcessingTapFlags *outFlags) {
 if(MTAudioProcessingTapGetSourceAudio(tap,frames,buffers,outFlags,NULL,outFrames)!=noErr)return;
 AKSpectrum *s=MTAudioProcessingTapGetStorage(tap);
 if(!s||!s->fft||s->format.mFormatID!=kAudioFormatLinearPCM||!(s->format.mFormatFlags&kAudioFormatFlagIsFloat)||s->format.mBitsPerChannel!=32)return;
 if((*outFlags&kMTAudioProcessingTapFlag_StartOfStream)!=0)s->count=0;
 for(CMItemCount i=0;i<*outFrames;i++){
  float sample=0;unsigned channels=0;
  for(UInt32 b=0;b<buffers->mNumberBuffers;b++){
   AudioBuffer buf=buffers->mBuffers[b];if(!buf.mData)continue;
   for(UInt32 c=0;c<buf.mNumberChannels;c++){sample+=((float *)buf.mData)[i*buf.mNumberChannels+c];channels++;}
  }
  s->samples[s->count++]=channels?sample/channels:0;
  if(s->count!=AK_FFT)continue;
  s->count=0;
  vDSP_vmul(s->samples,1,s->window,1,s->work,1,AK_FFT);
  DSPSplitComplex split={s->real,s->imag};
  vDSP_ctoz((DSPComplex *)s->work,2,&split,1,AK_FFT/2);
  vDSP_fft_zrip(s->fft,&split,1,11,FFT_FORWARD);s->imag[0]=0;
  vDSP_zvmags(&split,1,s->power,1,AK_FFT/2);
  float levels[AK_BANDS];double upper=fmin(16000,s->format.mSampleRate/2);
  for(int b=0;b<AK_BANDS;b++){
   int lo=fmax(1,floor(40*pow(upper/40,(double)b/AK_BANDS)*AK_FFT/s->format.mSampleRate));
   int hi=fmin(AK_FFT/2,fmax(lo+1,ceil(40*pow(upper/40,(double)(b+1)/AK_BANDS)*AK_FFT/s->format.mSampleRate)));
   float peak=0;for(int k=lo;k<hi;k++)peak=fmaxf(peak,s->power[k]);
   float db=10*log10f(fmaxf(peak/(AK_FFT*AK_FFT),1e-12f));
   levels[b]=fminf(1,fmaxf(0,(db+75)/65));
  }
  if(pthread_mutex_trylock(&s->lock)==0){memcpy(s->levels,levels,sizeof(levels));pthread_mutex_unlock(&s->lock);}
 }
}


__attribute__((used,section("__TEXT,__info_plist")))
static const char akariInfoPlist[] =
 "<?xml version='1.0' encoding='UTF-8'?>"
 "<plist version='1.0'><dict>"
 "<key>CFBundleIdentifier</key><string>com.eli.Akari</string>"
 "<key>CFBundleName</key><string>Akari</string>"
 "<key>CFBundleExecutable</key><string>akari</string>"
 "<key>CFBundleVersion</key><string>1</string>"
 "<key>NSAppTransportSecurity</key><dict><key>NSAllowsLocalNetworking</key><true/></dict>"
 "</dict></plist>";

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
- (AKSnapshot)snapshot;
@end

@implementation AKNativePlayer
- (instancetype)initWithSpatial:(BOOL)spatial {
 if((self=[super init])){
  _spatial=spatial;_paused=YES;_seekTarget=NAN;
  _player=[AVPlayer new];_player.automaticallyWaitsToMinimizeStalling=YES;_player.volume=0.7;
 }
 return self;
}
- (void)execute:(void (^)(void))work {
 if([NSThread isMainThread]){work();return;}
 dispatch_sync(dispatch_get_main_queue(),^{@autoreleasepool{work();}});
}
- (void)clearItem {
 if(_endObserver){[[NSNotificationCenter defaultCenter] removeObserver:_endObserver];_endObserver=nil;}
 [_player pause];[_player.currentItem cancelPendingSeeks];[_asset cancelLoading];
 [_player replaceCurrentItemWithPlayerItem:nil];if(_tap){CFRelease(_tap);_tap=NULL;}_asset=nil;_ready=NO;_seekTarget=NAN;
 atomic_store(&_endedGeneration,0);
}
- (void)load:(NSDictionary *)command {
 [self clearItem];
 if(_player.status==AVPlayerStatusFailed){float volume=_player.volume;_player=[AVPlayer new];_player.automaticallyWaitsToMinimizeStalling=YES;_player.volume=volume;}
 _generation=[command[@"generation"] unsignedLongLongValue];
 _paused=[command[@"paused"] boolValue];_initialPosition=[command[@"position"] doubleValue];
 NSString *source=command[@"source"];
 NSURL *url=[source hasPrefix:@"/"]?[NSURL fileURLWithPath:source]:[NSURL URLWithString:source];
 _asset=[AVURLAsset URLAssetWithURL:url options:nil];
 AVPlayerItem *item=[AVPlayerItem playerItemWithAsset:_asset];
 item.preferredForwardBufferDuration=2;
 item.allowedAudioSpatializationFormats=_spatial?AVAudioSpatializationFormatMonoStereoAndMultichannel:AVAudioSpatializationFormatNone;
 uint64_t generation=_generation;
 _endObserver=[[NSNotificationCenter defaultCenter] addObserverForName:AVPlayerItemDidPlayToEndTimeNotification object:item queue:nil usingBlock:^(NSNotification *note){
  if(atomic_load(&self->_generation)==generation)atomic_store(&self->_endedGeneration,generation);
 }];
 [_player replaceCurrentItemWithPlayerItem:item];
}
- (void)setPaused:(BOOL)paused {
 _paused=paused;
 if(paused){[_player pause];}else if(_ready){[_player play];}
}
- (void)setVolume:(double)volume {_player.volume=(float)(volume/100.0);}
- (void)setSpatial:(BOOL)enabled {
 _spatial=enabled;
 _player.currentItem.allowedAudioSpatializationFormats=enabled?AVAudioSpatializationFormatMonoStereoAndMultichannel:AVAudioSpatializationFormatNone;
}
- (void)seek:(double)value absolute:(BOOL)absolute {
 if(!_player.currentItem)return;
 double position=absolute?value:CMTimeGetSeconds(_player.currentTime)+value;
 if(!isfinite(position))position=0;
 double duration=CMTimeGetSeconds(_player.currentItem.duration);
 if(isfinite(duration)&&duration>0)position=fmin(position,duration);
 position=fmax(0,position);_seekTarget=position;
 atomic_store(&_endedGeneration,0);
 [_player seekToTime:CMTimeMakeWithSeconds(position,600) toleranceBefore:kCMTimeZero toleranceAfter:kCMTimeZero];
}
- (AKSnapshot)snapshot {
 AKSnapshot result={0};
 result.generation=_generation;result.paused=_paused;result.volume=round(_player.volume*10000.0)/100.0;result.spatial=_spatial;
 AVPlayerItem *item=_player.currentItem;
 if(!item)return result;
 if(item.status==AVPlayerItemStatusFailed||_player.status==AVPlayerStatusFailed){
  result.status=3;result.error_code=(int)(item.error?:_player.error).code;return result;
 }
 if(item.status!=AVPlayerItemStatusReadyToPlay){result.status=1;result.buffering=!_paused;return result;}
 if(!_ready){
  _ready=YES;
  for(AVPlayerItemTrack *track in item.tracks){if([track.assetTrack.mediaType isEqualToString:AVMediaTypeVideo])track.enabled=NO;}
  for(AVPlayerItemTrack *track in item.tracks){
   if(![track.assetTrack.mediaType isEqualToString:AVMediaTypeAudio])continue;
   MTAudioProcessingTapCallbacks callbacks;
   memset(&callbacks,0,sizeof(callbacks));callbacks.version=kMTAudioProcessingTapCallbacksVersion_0;
   callbacks.init=akTapInit;callbacks.finalize=akTapFinalize;callbacks.prepare=akTapPrepare;callbacks.unprepare=akTapUnprepare;callbacks.process=akTapProcess;
   if(MTAudioProcessingTapCreate(kCFAllocatorDefault,&callbacks,kMTAudioProcessingTapCreationFlag_PreEffects,&_tap)==noErr){
    AVMutableAudioMixInputParameters *params=[AVMutableAudioMixInputParameters audioMixInputParametersWithTrack:track.assetTrack];
    params.audioTapProcessor=_tap;
    AVMutableAudioMix *mix=[AVMutableAudioMix audioMix];mix.inputParameters=@[params];item.audioMix=mix;
   }
   break;
  }
  if(_initialPosition>0)[self seek:_initialPosition absolute:YES];
  if(!_paused)[_player play];
 }
 result.status=2;
 result.spatial=item.allowedAudioSpatializationFormats==AVAudioSpatializationFormatMonoStereoAndMultichannel;
 double position=CMTimeGetSeconds(_player.currentTime),duration=CMTimeGetSeconds(item.duration);
 if(!isfinite(position))position=0;
 if(isfinite(_seekTarget)){
  if(fabs(position-_seekTarget)<0.5)_seekTarget=NAN;
  else position=_seekTarget;
 }
 result.position=fmax(0,position);result.duration=isfinite(duration)?fmax(0,duration):0;
 result.buffering=!_paused&&_player.timeControlStatus==AVPlayerTimeControlStatusWaitingToPlayAtSpecifiedRate;
 result.ended=_generation!=0&&atomic_load(&_endedGeneration)==_generation;
 return result;
}
- (void)spectrum:(float *)levels {
 if(!_tap||_paused||_player.timeControlStatus!=AVPlayerTimeControlStatusPlaying)return;
 AKSpectrum *s=MTAudioProcessingTapGetStorage(_tap);if(!s)return;
 pthread_mutex_lock(&s->lock);memcpy(levels,s->levels,sizeof(s->levels));pthread_mutex_unlock(&s->lock);
}
- (void)stop:(uint64_t)generation {[self clearItem];_generation=generation;_paused=YES;}
- (void)shutdown {[self execute:^{[self clearItem];self->_player=nil;}];}

@end

static _Atomic(BOOL) akMainStopped=NO;
void ak_main_run(void){@autoreleasepool{
 CFRunLoopSourceContext context={0};
 CFRunLoopSourceRef source=CFRunLoopSourceCreate(NULL,0,&context);
 CFRunLoopAddSource(CFRunLoopGetMain(),source,kCFRunLoopDefaultMode);
 while(!atomic_load(&akMainStopped)){@autoreleasepool{CFRunLoopRunInMode(kCFRunLoopDefaultMode,0.25,true);}}
 CFRunLoopRemoveSource(CFRunLoopGetMain(),source,kCFRunLoopDefaultMode);CFRelease(source);
}}
void ak_main_stop(void){
 atomic_store(&akMainStopped,YES);
 CFRunLoopWakeUp(CFRunLoopGetMain());
}
void *ak_player_create(int spatial){@autoreleasepool{
 __block AKNativePlayer *player;
 void (^create)(void)=^{@autoreleasepool{player=[[AKNativePlayer alloc] initWithSpatial:spatial!=0];}};
 if([NSThread isMainThread]){create();}else{dispatch_sync(dispatch_get_main_queue(),create);}
 return (__bridge_retained void *)player;
}}
int ak_player_load(void *handle,const char *source,int paused,double position,uint64_t generation){@autoreleasepool{
 AKNativePlayer *native=(__bridge AKNativePlayer *)handle;
 NSString *path=[NSString stringWithUTF8String:source];if(!path)return 0;
 NSDictionary *command=@{@"source":path,@"paused":@(paused!=0),@"position":@(position),@"generation":@(generation)};
 [native execute:^{[native load:command];}];return 1;
}}
void ak_player_pause(void *handle,int paused){@autoreleasepool{AKNativePlayer *p=(__bridge AKNativePlayer *)handle;[p execute:^{[p setPaused:paused!=0];}];}}
void ak_player_seek(void *handle,double value,int absolute){@autoreleasepool{AKNativePlayer *p=(__bridge AKNativePlayer *)handle;[p execute:^{[p seek:value absolute:absolute!=0];}];}}
void ak_player_volume(void *handle,double volume){@autoreleasepool{AKNativePlayer *p=(__bridge AKNativePlayer *)handle;[p execute:^{[p setVolume:volume];}];}}
void ak_player_spatial(void *handle,int enabled){@autoreleasepool{AKNativePlayer *p=(__bridge AKNativePlayer *)handle;[p execute:^{[p setSpatial:enabled!=0];}];}}
void ak_player_stop(void *handle,uint64_t generation){@autoreleasepool{AKNativePlayer *p=(__bridge AKNativePlayer *)handle;[p execute:^{[p stop:generation];}];}}
AKSnapshot ak_player_snapshot(void *handle){@autoreleasepool{AKNativePlayer *p=(__bridge AKNativePlayer *)handle;__block AKSnapshot state;[p execute:^{state=[p snapshot];}];return state;}}
void ak_player_destroy(void *handle){@autoreleasepool{AKNativePlayer *p=CFBridgingRelease(handle);[p shutdown];}}

void ak_player_spectrum(void *handle,float *levels){@autoreleasepool{AKNativePlayer *p=(__bridge AKNativePlayer *)handle;[p execute:^{[p spectrum:levels];}];}}
