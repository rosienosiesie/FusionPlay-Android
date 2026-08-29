package com.fusionplay.miplaylyricsprobe;

import android.app.Activity;
import android.media.AudioAttributes;
import android.media.AudioFormat;
import android.media.AudioManager;
import android.media.AudioTrack;
import android.media.MediaMetadata;
import android.media.session.MediaSession;
import android.media.session.PlaybackState;
import android.os.Bundle;
import android.os.SystemClock;
import android.view.Gravity;
import android.widget.TextView;

import java.util.Locale;

/**
 * Debug-only sender used to prove that lyrics reach FusionPlay through MiPlay's
 * own SET_MEDIA_INFO/mLrc field. It never performs title matching or network IO.
 */
public final class MainActivity extends Activity {
    private static final int SAMPLE_RATE = 48_000;
    private static final int TONE_SECONDS = 8;
    private static final long DURATION_MS = 24_000L;
    private static final String[] TITLES = {
        "MiPlay 发送端歌词探针 A",
        "MiPlay 发送端歌词探针 B",
    };
    private static final String[] LYRICS = {
        "[offset:0]\n[00:00.00]歌词来自发送端 MediaSession\n[00:03.00]由 HyperOS MiPlay 编码为 mLrc\n[00:06.00]FusionPlay 只负责接收与显示\n[00:09.00]没有本地或在线歌词匹配",
        "[offset:0]\n[00:00.00]第二首发送端测试歌词\n[00:03.00]上一曲与下一曲会更新元数据\n[00:06.00]接收端应清除上一首歌词\n[00:09.00]并显示当前时间行",
    };

    private MediaSession mediaSession;
    private AudioTrack audioTrack;
    private TextView statusView;
    private int trackIndex;
    private long positionMs;
    private long playStartedRealtimeMs;
    private boolean playing;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        statusView = new TextView(this);
        statusView.setGravity(Gravity.CENTER);
        statusView.setTextSize(20f);
        statusView.setPadding(48, 48, 48, 48);
        setContentView(statusView);

        createAudioTrack();
        createMediaSession();
        publishMetadata();
        play();
    }

    private void createAudioTrack() {
        int frameCount = SAMPLE_RATE * TONE_SECONDS;
        short[] samples = new short[frameCount];
        for (int frame = 0; frame < frameCount; frame++) {
            double phase = 2.0 * Math.PI * 440.0 * frame / SAMPLE_RATE;
            samples[frame] = (short) (Math.sin(phase) * Short.MAX_VALUE * 0.04);
        }
        audioTrack = new AudioTrack.Builder()
            .setAudioAttributes(new AudioAttributes.Builder()
                .setUsage(AudioAttributes.USAGE_MEDIA)
                .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                .build())
            .setAudioFormat(new AudioFormat.Builder()
                .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                .setSampleRate(SAMPLE_RATE)
                .setChannelMask(AudioFormat.CHANNEL_OUT_MONO)
                .build())
            .setTransferMode(AudioTrack.MODE_STATIC)
            .setBufferSizeInBytes(samples.length * 2)
            .build();
        audioTrack.write(samples, 0, samples.length);
        audioTrack.setLoopPoints(0, samples.length, -1);
    }

    private void createMediaSession() {
        mediaSession = new MediaSession(this, "FusionPlayMiPlayLyricsProbe");
        mediaSession.setFlags(
            MediaSession.FLAG_HANDLES_MEDIA_BUTTONS |
                MediaSession.FLAG_HANDLES_TRANSPORT_CONTROLS
        );
        mediaSession.setCallback(new MediaSession.Callback() {
            @Override
            public void onPlay() {
                play();
            }

            @Override
            public void onPause() {
                pause();
            }

            @Override
            public void onSeekTo(long pos) {
                positionMs = Math.max(0L, Math.min(pos, DURATION_MS));
                if (playing) {
                    playStartedRealtimeMs = SystemClock.elapsedRealtime();
                }
                publishPlaybackState();
            }

            @Override
            public void onSkipToNext() {
                selectTrack((trackIndex + 1) % TITLES.length);
            }

            @Override
            public void onSkipToPrevious() {
                selectTrack((trackIndex + TITLES.length - 1) % TITLES.length);
            }
        });
        mediaSession.setActive(true);
    }

    private void selectTrack(int index) {
        trackIndex = index;
        positionMs = 0L;
        playStartedRealtimeMs = SystemClock.elapsedRealtime();
        publishMetadata();
        publishPlaybackState();
    }

    private void publishMetadata() {
        mediaSession.setMetadata(new MediaMetadata.Builder()
            .putString(MediaMetadata.METADATA_KEY_MEDIA_ID, "fusionplay-lyrics-probe-" + trackIndex)
            .putString(MediaMetadata.METADATA_KEY_TITLE, TITLES[trackIndex])
            .putString(MediaMetadata.METADATA_KEY_ARTIST, "FusionPlay 诊断发送端")
            .putString(MediaMetadata.METADATA_KEY_ALBUM, "MiPlay 原生歌词传输验证")
            .putString(MediaMetadata.METADATA_KEY_DISPLAY_DESCRIPTION, LYRICS[trackIndex])
            .putLong(MediaMetadata.METADATA_KEY_DURATION, DURATION_MS)
            .build());
        updateStatusText();
    }

    private void play() {
        if (playing || audioTrack == null) {
            return;
        }
        playStartedRealtimeMs = SystemClock.elapsedRealtime();
        audioTrack.play();
        playing = true;
        publishPlaybackState();
    }

    private void pause() {
        if (!playing || audioTrack == null) {
            return;
        }
        positionMs = currentPositionMs();
        audioTrack.pause();
        playing = false;
        publishPlaybackState();
    }

    private long currentPositionMs() {
        if (!playing) {
            return positionMs;
        }
        long elapsed = SystemClock.elapsedRealtime() - playStartedRealtimeMs;
        return (positionMs + elapsed) % DURATION_MS;
    }

    private void publishPlaybackState() {
        long actions = PlaybackState.ACTION_PLAY |
            PlaybackState.ACTION_PAUSE |
            PlaybackState.ACTION_PLAY_PAUSE |
            PlaybackState.ACTION_SEEK_TO |
            PlaybackState.ACTION_SKIP_TO_NEXT |
            PlaybackState.ACTION_SKIP_TO_PREVIOUS;
        int state = playing ? PlaybackState.STATE_PLAYING : PlaybackState.STATE_PAUSED;
        mediaSession.setPlaybackState(new PlaybackState.Builder()
            .setActions(actions)
            .setState(state, currentPositionMs(), playing ? 1f : 0f)
            .build());
        updateStatusText();
    }

    private void updateStatusText() {
        if (statusView == null) {
            return;
        }
        statusView.setText(String.format(
            Locale.ROOT,
            "%s\n\n%s\n\n歌词已写入\nandroid.media.metadata.DISPLAY_DESCRIPTION\n\n请从 HyperOS 妙播选择 FusionPlay",
            playing ? "正在播放" : "已暂停",
            TITLES[trackIndex]
        ));
    }

    @Override
    protected void onDestroy() {
        if (mediaSession != null) {
            mediaSession.setActive(false);
            mediaSession.release();
            mediaSession = null;
        }
        if (audioTrack != null) {
            audioTrack.stop();
            audioTrack.release();
            audioTrack = null;
        }
        super.onDestroy();
    }
}
