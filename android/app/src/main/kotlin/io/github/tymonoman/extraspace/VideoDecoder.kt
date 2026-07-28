package io.github.tymonoman.extraspace

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong
import kotlin.concurrent.thread

/**
 * Hardware H.264 decode straight onto a [Surface].
 *
 * Frames are handed to the codec as they arrive and released as soon as they are
 * decoded. Nothing is queued deliberately: for a live second display a late frame
 * is worthless, so the goal is to keep the codec's input queue as close to empty
 * as possible and report its depth back to the host, which lowers bitrate when it
 * starts to grow.
 */
class VideoDecoder(private val surface: Surface) {
    private var codec: MediaCodec? = null
    @Volatile private var running = false
    private var drainThread: Thread? = null

    /** Codec input queue depth -- the host's main signal that we are falling behind. */
    val queueDepth: Int get() = pendingInputs.get()
    val framesDecoded = AtomicLong(0)
    val framesDropped = AtomicLong(0)
    /** Device-clock microseconds when the most recent frame was released for display. */
    val renderedAtUs = AtomicLong(0)
    val lastFramePtsUs = AtomicLong(0)

    /** Frames submitted but not yet released for display. Touched by two threads. */
    private val pendingInputs = AtomicInteger(0)

    fun start(width: Int, height: Int, csd: ByteArray?) {
        stop()
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height).apply {
            // Tells the decoder to minimise internal buffering. Without this most
            // MediaCodec implementations hold 2-4 frames, which alone can exceed
            // our entire latency budget.
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                setInteger(MediaFormat.KEY_PRIORITY, 0) // realtime
            }
            // SPS/PPS, if the host sent them ahead of the first frame. The host
            // also repeats them inline on every keyframe, so this is belt and
            // braces for the very first connection.
            csd?.let { setByteBuffer("csd-0", ByteBuffer.wrap(it)) }
        }

        codec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC).apply {
            configure(format, surface, null, 0)
            start()
        }
        running = true
        // Output is drained on its own thread rather than piggybacking on input.
        // Draining only when a new frame arrives means that when the desktop goes
        // idle -- which, since mutter only sends on damage, is most of the time --
        // the last frame sits decoded but unrendered until something else changes.
        drainThread = thread(name = "xs-decode-drain") { drainLoop() }
        Log.i(TAG, "decoder started ${width}x$height low-latency=${Build.VERSION.SDK_INT >= Build.VERSION_CODES.R}")
    }

    /**
     * Submits one access unit. Returns false if the codec could not accept it,
     * which means we are behind and the frame is discarded.
     */
    fun decode(data: ByteArray, length: Int, ptsUs: Long, isConfig: Boolean): Boolean {
        val mc = codec ?: return false
        return try {
            // Short timeout rather than blocking: if the codec is saturated we
            // would rather drop this frame than stall the socket reader and let
            // even more frames pile up behind it.
            val index = mc.dequeueInputBuffer(INPUT_TIMEOUT_US)
            if (index < 0) {
                framesDropped.incrementAndGet()
                return false
            }
            mc.getInputBuffer(index)?.apply {
                clear()
                put(data, 0, length)
            }
            val flags = if (isConfig) MediaCodec.BUFFER_FLAG_CODEC_CONFIG else 0
            mc.queueInputBuffer(index, 0, length, ptsUs, flags)
            pendingInputs.incrementAndGet()
            lastFramePtsUs.set(ptsUs)
            true
        } catch (e: IllegalStateException) {
            Log.e(TAG, "decoder rejected input", e)
            false
        }
    }

    /**
     * Releases finished frames to the surface as soon as they are ready.
     *
     * Blocks on the codec rather than spinning, so an idle desktop costs nothing
     * while a frame that does arrive is rendered immediately.
     */
    private fun drainLoop() {
        val info = MediaCodec.BufferInfo()
        while (running) {
            val mc = codec ?: break
            try {
                when (val index = mc.dequeueOutputBuffer(info, DRAIN_TIMEOUT_US)) {
                    in 0..Int.MAX_VALUE -> {
                        // true = render this frame to the surface now.
                        mc.releaseOutputBuffer(index, true)
                        pendingInputs.updateAndGet { (it - 1).coerceAtLeast(0) }
                        framesDecoded.incrementAndGet()
                        renderedAtUs.set(System.nanoTime() / 1000)
                    }
                    MediaCodec.INFO_OUTPUT_FORMAT_CHANGED ->
                        Log.i(TAG, "output format now ${mc.outputFormat}")
                    else -> {} // INFO_TRY_AGAIN_LATER: nothing ready, loop again
                }
            } catch (e: IllegalStateException) {
                if (running) Log.e(TAG, "decoder drain failed", e)
                break
            }
        }
    }

    fun stop() {
        running = false
        drainThread?.join(500)
        drainThread = null
        codec?.let {
            runCatching { it.stop() }
            runCatching { it.release() }
        }
        codec = null
        pendingInputs.set(0)
    }

    private companion object {
        const val TAG = "extraspace"
        const val INPUT_TIMEOUT_US = 10_000L

        /// Long enough that an idle desktop does not spin the CPU, short enough
        /// that shutdown is not noticeably delayed waiting for this to return.
        const val DRAIN_TIMEOUT_US = 20_000L
    }
}
