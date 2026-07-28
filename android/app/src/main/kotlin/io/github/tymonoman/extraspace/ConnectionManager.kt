package io.github.tymonoman.extraspace

import android.net.LocalServerSocket
import android.net.LocalSocket
import android.os.Build
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject
import java.io.Closeable
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread

/**
 * Owns the three abstract unix sockets the host connects to.
 *
 * The tablet listens and the host connects, rather than the other way round. That
 * ordering means the app can be launched and simply wait, and it survives the host
 * reconnecting without needing the app to be restarted.
 *
 * Each socket gets its own thread. They are almost entirely independent, and
 * keeping video off the same thread as control means a slow decode can never
 * delay a touch event.
 */
class ConnectionManager(
    private val callbacks: Callbacks,
) : Closeable {

    interface Callbacks {
        /** Host has sent the stream parameters; set up the decoder. */
        fun onVideoConfig(width: Int, height: Int, framerate: Int)
        /** One access unit arrived. */
        fun onVideoFrame(data: ByteArray, length: Int, ptsUs: Long, isConfig: Boolean)
        /** Host asked us to start or stop the camera. */
        fun onCameraControl(enabled: Boolean, cameraId: String, width: Int, height: Int, framerate: Int, bitrateKbps: Int)
        fun onConnected()
        fun onDisconnected(reason: String)
    }

    private val running = AtomicBoolean(false)
    private var controlServer: LocalServerSocket? = null
    private var videoServer: LocalServerSocket? = null
    private var cameraServer: LocalServerSocket? = null

    private var controlSocket: LocalSocket? = null
    private var cameraSocket: LocalSocket? = null

    @Volatile private var controlWriter: FrameWriter? = null
    @Volatile private var cameraWriter: FrameWriter? = null

    fun start() {
        if (!running.compareAndSet(false, true)) return
        controlServer = LocalServerSocket(Protocol.Sockets.CONTROL)
        videoServer = LocalServerSocket(Protocol.Sockets.VIDEO)
        cameraServer = LocalServerSocket(Protocol.Sockets.CAMERA)

        thread(name = "xs-control") { runControl() }
        thread(name = "xs-video") { runVideo() }
        thread(name = "xs-camera") { runCamera() }
        Log.i(TAG, "listening on all three sockets")
    }

    // ------------------------------------------------------------- control
    private fun runControl() {
        try {
            val socket = controlServer!!.accept()
            controlSocket = socket
            val reader = FrameReader(socket.inputStream)
            val writer = FrameWriter(socket.outputStream)
            controlWriter = writer

            sendHello(writer)
            callbacks.onConnected()

            while (running.get()) {
                val header = reader.readHeader()
                val payload = reader.readPayload(header)
                when (header.kind) {
                    Protocol.ControlKind.VIDEO_CONFIG -> {
                        val json = JSONObject(String(payload))
                        callbacks.onVideoConfig(
                            json.getInt("width"),
                            json.getInt("height"),
                            json.getInt("framerate"),
                        )
                    }
                    Protocol.ControlKind.CAMERA_CONTROL -> {
                        val json = JSONObject(String(payload))
                        callbacks.onCameraControl(
                            json.getBoolean("enabled"),
                            json.optString("camera_id", "0"),
                            json.optInt("width", 1920),
                            json.optInt("height", 1080),
                            json.optInt("framerate", 30),
                            json.optInt("bitrate_kbps", 8000),
                        )
                    }
                    Protocol.ControlKind.PING -> {
                        // Echo the timestamp back untouched so the host can measure
                        // a true round trip without us needing a synced clock.
                        writer.write(
                            Protocol.Channel.CONTROL, Protocol.ControlKind.PONG,
                            0, header.ptsUs, ByteArray(0),
                        )
                    }
                    else -> Log.w(TAG, "unhandled control kind ${header.kind}")
                }
            }
        } catch (e: Exception) {
            if (running.get()) {
                Log.e(TAG, "control channel failed", e)
                callbacks.onDisconnected(e.message ?: "control channel closed")
            }
        }
    }

    private fun sendHello(writer: FrameWriter) {
        val json = JSONObject().apply {
            put("protocol_version", Protocol.VERSION)
            put("device_name", "${Build.MANUFACTURER} ${Build.MODEL}")
            put("android_release", Build.VERSION.RELEASE)
            put("width", callbacks.let { DeviceInfo.width })
            put("height", DeviceInfo.height)
            put("density_dpi", DeviceInfo.densityDpi)
            put("refresh_rate", DeviceInfo.refreshRate)
            put("cameras", JSONArray().apply {
                DeviceInfo.cameras.forEach { cam ->
                    put(JSONObject().apply {
                        put("id", cam.id)
                        put("facing", cam.facing)
                        put("max_width", cam.maxWidth)
                        put("max_height", cam.maxHeight)
                    })
                }
            })
        }
        writer.write(
            Protocol.Channel.CONTROL, Protocol.ControlKind.HELLO,
            0, 0, json.toString().toByteArray(),
        )
        Log.i(TAG, "sent hello: ${DeviceInfo.width}x${DeviceInfo.height} @${DeviceInfo.refreshRate}")
    }

    /** Sends a touch event. Cheap enough to call straight from the input thread. */
    fun sendTouch(event: TouchEvent) {
        val writer = controlWriter ?: return
        try {
            writer.write(
                Protocol.Channel.TOUCH, 0, 0,
                System.nanoTime() / 1000, event.encode(),
            )
        } catch (e: Exception) {
            Log.w(TAG, "could not send touch", e)
        }
    }

    /** Periodic health report that drives the host's adaptive bitrate controller. */
    fun sendStats(queueDepth: Int, decoded: Long, dropped: Long, lastPtsUs: Long, renderedAtUs: Long) {
        val writer = controlWriter ?: return
        val json = JSONObject().apply {
            put("decode_queue_depth", queueDepth)
            put("frames_decoded", decoded)
            put("frames_dropped", dropped)
            put("last_frame_pts_us", lastPtsUs)
            put("rendered_at_us", renderedAtUs)
        }
        try {
            writer.write(
                Protocol.Channel.CONTROL, Protocol.ControlKind.STATS,
                0, System.nanoTime() / 1000, json.toString().toByteArray(),
            )
        } catch (e: Exception) {
            Log.w(TAG, "could not send stats", e)
        }
    }

    // --------------------------------------------------------------- video
    private fun runVideo() {
        try {
            val socket = videoServer!!.accept()
            val reader = FrameReader(socket.inputStream)
            // One reusable buffer: at 60fps, allocating per frame would keep the
            // GC busy for no reason.
            var buf = ByteArray(512 * 1024)

            while (running.get()) {
                val header = reader.readHeader()
                if (header.length > buf.size) buf = ByteArray(header.length.coerceAtLeast(buf.size * 2))
                reader.readPayload(header, buf)
                val isConfig = (header.flags.toInt() and Protocol.Flags.CODEC_CONFIG.toInt()) != 0
                callbacks.onVideoFrame(buf, header.length, header.ptsUs, isConfig)
            }
        } catch (e: Exception) {
            if (running.get()) {
                Log.e(TAG, "video channel failed", e)
                callbacks.onDisconnected(e.message ?: "video channel closed")
            }
        }
    }

    // -------------------------------------------------------------- camera
    private fun runCamera() {
        try {
            val socket = cameraServer!!.accept()
            cameraSocket = socket
            cameraWriter = FrameWriter(socket.outputStream)
            Log.i(TAG, "camera channel connected")
            // Host-bound only; nothing to read. Park until shutdown so the
            // socket stays open.
            while (running.get()) Thread.sleep(1000)
        } catch (e: Exception) {
            if (running.get()) Log.e(TAG, "camera channel failed", e)
        }
    }

    /** Sends one encoded camera access unit to the host. */
    fun sendCameraFrame(data: ByteArray, length: Int, ptsUs: Long, isConfig: Boolean, isKeyframe: Boolean) {
        val writer = cameraWriter ?: return
        var flags = 0
        if (isConfig) flags = flags or Protocol.Flags.CODEC_CONFIG.toInt()
        if (isKeyframe) flags = flags or Protocol.Flags.KEYFRAME.toInt()
        try {
            writer.write(Protocol.Channel.CAMERA_UP, 0, flags.toShort(), ptsUs, data, length)
        } catch (e: Exception) {
            Log.w(TAG, "could not send camera frame", e)
        }
    }

    override fun close() {
        running.set(false)
        listOf<Closeable?>(controlServer, videoServer, cameraServer, controlSocket, cameraSocket)
            .forEach { runCatching { it?.close() } }
        controlWriter = null
        cameraWriter = null
    }

    private companion object {
        const val TAG = "extraspace"
    }
}
