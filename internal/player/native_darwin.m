#import "native.h"
#import <AVFoundation/AVFoundation.h>
#import <Foundation/Foundation.h>
#import <CoreMedia/CoreMedia.h>
#import <stdatomic.h>
#import <math.h>

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
 [_player replaceCurrentItemWithPlayerItem:nil];_asset=nil;_ready=NO;_seekTarget=NAN;
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
