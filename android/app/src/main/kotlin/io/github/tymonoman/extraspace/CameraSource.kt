package io.github.tymonoman.extraspace

import android.Manifest
import android.annotation.SuppressLint
import android.content.Context
import android.content.pm.PackageManager
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.util.Range
import android.view.Surface
import androidx.core.content.ContextCompat
import kotlin.concurrent.thread

/**
 * Camera2 capture encoded straight to H.264 and streamed to the host, which
 * writes it into a v4l2loopback device so it appears as an ordinary webcam.
 *
 * The camera renders directly into the encoder's input surface, so frames never
 * touch the CPU or the Java heap on the way through.
 */
class CameraSource(
    private val context: Context,
    private val onFrame: (data: ByteArray, length: Int, ptsUs: Long, isConfig: Boolean, isKeyframe: Boolean) -> Unit,
) {
    private var device: CameraDevice? = null
    private var session: CameraCaptureSession? = null
    private var encoder: MediaCodec? = null
    private var inputSurface: Surface? = null
    private var thread: HandlerThread? = null
    private var handler: Handler? = null
    @Volatile private var running = false

    fun hasPermission(): Boolean =
        ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) ==
            PackageManager.PERMISSION_GRANTED

    @SuppressLint("MissingPermission")
    fun start(cameraId: String, width: Int, height: Int, framerate: Int, bitrateKbps: Int) {
        if (!hasPermission()) {
            Log.e(TAG, "camera permission not granted; ask for it before starting")
            return
        }
        stop()
        running = true

        thread = HandlerThread("xs-camera").also { it.start() }
        handler = Handler(thread!!.looper)

        startEncoder(width, height, framerate, bitrateKbps)

        val manager = context.getSystemService(CameraManager::class.java)
        manager.openCamera(cameraId, object : CameraDevice.StateCallback() {
            override fun onOpened(cam: CameraDevice) {
                device = cam
                createSession(cam, framerate)
            }

            override fun onDisconnected(cam: CameraDevice) {
                Log.w(TAG, "camera disconnected")
                cam.close()
                device = null
            }

            override fun onError(cam: CameraDevice, error: Int) {
                Log.e(TAG, "camera error $error")
                cam.close()
                device = null
            }
        }, handler)
    }

    private fun startEncoder(width: Int, height: Int, framerate: Int, bitrateKbps: Int) {
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height).apply {
            setInteger(
                MediaFormat.KEY_COLOR_FORMAT,
                MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface,
            )
            setInteger(MediaFormat.KEY_BIT_RATE, bitrateKbps * 1000)
            setInteger(MediaFormat.KEY_FRAME_RATE, framerate)
            // A keyframe every second: a webcam consumer may attach at any moment
            // and cannot show anything until it sees one.
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 1)
            setInteger(MediaFormat.KEY_BITRATE_MODE, MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR)
        }

        encoder = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC).apply {
            configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            inputSurface = createInputSurface()
            start()
        }
        thread(name = "xs-camera-drain") { drainEncoder() }
        Log.i(TAG, "camera encoder started ${width}x$height @$framerate ${bitrateKbps}kbps")
    }

    private fun createSession(cam: CameraDevice, framerate: Int) {
        val surface = inputSurface ?: return
        val request = cam.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply {
            addTarget(surface)
            set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, Range(framerate, framerate))
            set(CaptureRequest.CONTROL_AF_MODE, CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_VIDEO)
        }.build()

        @Suppress("DEPRECATION")
        cam.createCaptureSession(listOf(surface), object : CameraCaptureSession.StateCallback() {
            override fun onConfigured(s: CameraCaptureSession) {
                session = s
                runCatching { s.setRepeatingRequest(request, null, handler) }
                    .onFailure { Log.e(TAG, "could not start repeating request", it) }
            }

            override fun onConfigureFailed(s: CameraCaptureSession) {
                Log.e(TAG, "capture session configuration failed")
            }
        }, handler)
    }

    private fun drainEncoder() {
        val info = MediaCodec.BufferInfo()
        while (running) {
            val mc = encoder ?: break
            try {
                val index = mc.dequeueOutputBuffer(info, DRAIN_TIMEOUT_US)
                if (index < 0) continue
                val buffer = mc.getOutputBuffer(index)
                if (buffer != null && info.size > 0) {
                    val data = ByteArray(info.size)
                    buffer.position(info.offset)
                    buffer.get(data, 0, info.size)
                    val isConfig = (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG) != 0
                    val isKey = (info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME) != 0
                    onFrame(data, info.size, info.presentationTimeUs, isConfig, isKey)
                }
                mc.releaseOutputBuffer(index, false)
            } catch (e: IllegalStateException) {
                if (running) Log.e(TAG, "encoder drain failed", e)
                break
            }
        }
    }

    fun stop() {
        running = false
        runCatching { session?.stopRepeating() }
        runCatching { session?.close() }
        runCatching { device?.close() }
        runCatching { encoder?.stop() }
        runCatching { encoder?.release() }
        runCatching { inputSurface?.release() }
        thread?.quitSafely()
        session = null
        device = null
        encoder = null
        inputSurface = null
        thread = null
        handler = null
    }

    private companion object {
        const val TAG = "extraspace"
        const val DRAIN_TIMEOUT_US = 10_000L
    }
}
