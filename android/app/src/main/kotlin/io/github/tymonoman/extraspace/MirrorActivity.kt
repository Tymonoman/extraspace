package io.github.tymonoman.extraspace

import android.annotation.SuppressLint
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowManager
import android.widget.TextView
import androidx.activity.ComponentActivity
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat

/**
 * The screen the tablet actually shows: a full-bleed [SurfaceView] fed by
 * [VideoDecoder], with touches forwarded back to the host.
 *
 * Deliberately not a Compose UI. Everything here is one surface and one gesture
 * stream, and Compose would add a recomposition layer between the touch event and
 * the socket for no benefit.
 */
class MirrorActivity : ComponentActivity(), ConnectionManager.Callbacks {

    private lateinit var surfaceView: SurfaceView
    private lateinit var statusView: TextView
    private var decoder: VideoDecoder? = null
    private var connection: ConnectionManager? = null
    private var camera: CameraSource? = null
    private val main = Handler(Looper.getMainLooper())

    /** Dimensions of the incoming stream; touches are mapped into this space. */
    private var streamWidth = 0
    private var streamHeight = 0

    private val statsTicker = object : Runnable {
        override fun run() {
            decoder?.let { d ->
                connection?.sendStats(
                    d.queueDepth,
                    d.framesDecoded.get(),
                    d.framesDropped.get(),
                    d.lastFramePtsUs.get(),
                    d.renderedAtUs.get(),
                )
            }
            main.postDelayed(this, STATS_INTERVAL_MS)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        DeviceInfo.load(this)

        // A second monitor that sleeps is not a second monitor.
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        goFullscreen()

        setContentView(R.layout.activity_mirror)
        surfaceView = findViewById(R.id.surface)
        statusView = findViewById(R.id.status)

        surfaceView.holder.addCallback(object : SurfaceHolder.Callback {
            override fun surfaceCreated(holder: SurfaceHolder) {
                decoder = VideoDecoder(holder.surface)
                startConnection()
            }

            override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) = Unit

            override fun surfaceDestroyed(holder: SurfaceHolder) {
                decoder?.stop()
                decoder = null
            }
        })
    }

    private fun startConnection() {
        if (connection != null) return
        showStatus(getString(R.string.waiting_for_host))
        connection = ConnectionManager(this).also { it.start() }
        main.postDelayed(statsTicker, STATS_INTERVAL_MS)
    }

    private fun goFullscreen() {
        WindowCompat.setDecorFitsSystemWindows(window, false)
        WindowInsetsControllerCompat(window, window.decorView).apply {
            hide(WindowInsetsCompat.Type.systemBars())
            systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
    }

    /** Safe to call from any thread; hops to the main thread itself. */
    private fun showStatus(text: String?) {
        main.post {
            statusView.text = text ?: ""
            statusView.visibility = if (text == null) View.GONE else View.VISIBLE
        }
    }

    // ------------------------------------------------------- host callbacks
    // All of these arrive on socket threads, so anything touching a view or the
    // decoder is posted to the main thread.
    override fun onVideoConfig(width: Int, height: Int, framerate: Int) {
        main.post {
            streamWidth = width
            streamHeight = height
            decoder?.start(width, height, null)
            Log.i(TAG, "stream configured ${width}x$height @$framerate")
        }
    }

    override fun onVideoFrame(data: ByteArray, length: Int, ptsUs: Long, isConfig: Boolean) {
        // Called on the video thread; MediaCodec is happy to be driven from here
        // and hopping to the main thread would only add latency.
        if (decoder == null) return
        if (streamWidth == 0) return
        decoder?.decode(data, length, ptsUs, isConfig)
        if (statusView.visibility == View.VISIBLE) showStatus(null)
    }

    override fun onCameraControl(
        enabled: Boolean, cameraId: String, width: Int, height: Int, framerate: Int, bitrateKbps: Int,
    ) {
        main.post {
            camera?.stop()
            camera = if (enabled) {
                CameraSource(this) { data, length, ptsUs, isConfig, isKey ->
                    connection?.sendCameraFrame(data, length, ptsUs, isConfig, isKey)
                }.also { it.start(cameraId, width, height, framerate, bitrateKbps) }
            } else {
                null
            }
        }
    }

    override fun onConnected() {
        showStatus(null)
    }

    override fun onDisconnected(reason: String) {
        Log.w(TAG, "disconnected: $reason")
        showStatus(getString(R.string.disconnected, reason))
    }

    // --------------------------------------------------------------- touch
    @SuppressLint("ClickableViewAccessibility")
    override fun onTouchEvent(event: MotionEvent): Boolean {
        val conn = connection ?: return false
        if (streamWidth == 0 || streamHeight == 0) return false

        // The surface may be letterboxed if the host's monitor aspect ratio does
        // not exactly match the panel, so map through the displayed rectangle
        // rather than assuming the view fills the screen.
        val viewW = surfaceView.width.toFloat()
        val viewH = surfaceView.height.toFloat()
        if (viewW <= 0f || viewH <= 0f) return false
        val scale = minOf(viewW / streamWidth, viewH / streamHeight)
        val offsetX = (viewW - streamWidth * scale) / 2f
        val offsetY = (viewH - streamHeight * scale) / 2f

        fun mapX(raw: Float) = ((raw - offsetX) / scale).toDouble().coerceIn(0.0, streamWidth - 1.0)
        fun mapY(raw: Float) = ((raw - offsetY) / scale).toDouble().coerceIn(0.0, streamHeight - 1.0)

        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
                val i = event.actionIndex
                conn.sendTouch(
                    TouchEvent(
                        Protocol.TouchAction.DOWN, event.getPointerId(i),
                        mapX(event.getX(i)), mapY(event.getY(i)),
                    )
                )
            }
            MotionEvent.ACTION_MOVE -> {
                // A MOVE batches every pointer that changed, so emit one per finger.
                for (i in 0 until event.pointerCount) {
                    conn.sendTouch(
                        TouchEvent(
                            Protocol.TouchAction.MOTION, event.getPointerId(i),
                            mapX(event.getX(i)), mapY(event.getY(i)),
                        )
                    )
                }
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP, MotionEvent.ACTION_CANCEL -> {
                val i = event.actionIndex
                conn.sendTouch(
                    TouchEvent(
                        Protocol.TouchAction.UP, event.getPointerId(i),
                        mapX(event.getX(i)), mapY(event.getY(i)),
                    )
                )
            }
        }
        return true
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) goFullscreen()
    }

    override fun onDestroy() {
        main.removeCallbacks(statsTicker)
        camera?.stop()
        connection?.close()
        decoder?.stop()
        super.onDestroy()
    }

    private companion object {
        const val TAG = "extraspace"
        const val STATS_INTERVAL_MS = 500L
    }
}
