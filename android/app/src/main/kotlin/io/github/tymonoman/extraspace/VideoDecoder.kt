package io.github.tymonoman.extraspace

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer
import java.util.concurrent.atomic.AtomicLong

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
    private val bufferInfo = MediaCodec.BufferInfo()

    /** Codec input queue depth -- the host's main signal that we are falling behind. */
    @Volatile var queueDepth: Int = 0; private set
    val framesDecoded = AtomicLong(0)
    val framesDropped = AtomicLong(0)
    /** Device-clock microseconds when the most recent frame was released for display. */
    val renderedAtUs = AtomicLong(0)
    val lastFramePtsUs = AtomicLong(0)

    private var pendingInputs = 0

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
            pendingInputs++
            queueDepth = pendingInputs
            lastFramePtsUs.set(ptsUs)
            drainOutput()
            true
        } catch (e: IllegalStateException) {
            Log.e(TAG, "decoder rejected input", e)
            false
        }
    }

    /** Releases everything the codec has finished with, rendering it to the surface. */
    private fun drainOutput() {
        val mc = codec ?: return
        while (true) {
            val index = mc.dequeueOutputBuffer(bufferInfo, 0)
            when {
                index >= 0 -> {
                    // true = render this frame to the surface now.
                    mc.releaseOutputBuffer(index, true)
                    pendingInputs = (pendingInputs - 1).coerceAtLeast(0)
                    framesDecoded.incrementAndGet()
                    renderedAtUs.set(System.nanoTime() / 1000)
                }
                index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                    Log.i(TAG, "output format now ${mc.outputFormat}")
                }
                else -> break // INFO_TRY_AGAIN_LATER or no output ready
            }
        }
        queueDepth = pendingInputs
    }

    fun stop() {
        codec?.let {
            runCatching { it.stop() }
            runCatching { it.release() }
        }
        codec = null
        pendingInputs = 0
        queueDepth = 0
    }

    private companion object {
        const val TAG = "extraspace"
        const val INPUT_TIMEOUT_US = 10_000L
    }
}
