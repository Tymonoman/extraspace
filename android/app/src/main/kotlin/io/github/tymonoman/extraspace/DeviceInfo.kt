package io.github.tymonoman.extraspace

import android.content.Context
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraManager
import android.hardware.camera2.params.StreamConfigurationMap
import android.os.Build
import android.util.Log

/**
 * Panel and camera facts the host needs in the `Hello` message.
 *
 * The host sizes the virtual monitor from [width]/[height], so these must be the
 * true panel dimensions in the orientation we actually display in -- getting it
 * wrong means a letterboxed or cropped desktop.
 */
object DeviceInfo {
    data class Camera(val id: String, val facing: String, val maxWidth: Int, val maxHeight: Int)

    var width: Int = 1920; private set
    var height: Int = 1080; private set
    var densityDpi: Int = 160; private set
    var refreshRate: Double = 60.0; private set
    var cameras: List<Camera> = emptyList(); private set

    fun load(context: Context) {
        loadDisplay(context)
        loadCameras(context)
        Log.i(TAG, "device: ${width}x$height @${refreshRate}Hz, ${densityDpi}dpi, ${cameras.size} cameras")
    }

    private fun loadDisplay(context: Context) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val metrics = context.getSystemService(android.view.WindowManager::class.java)
                .maximumWindowMetrics
            // Landscape is how a tablet is used as a monitor, so normalise to it
            // regardless of the device's natural orientation.
            width = maxOf(metrics.bounds.width(), metrics.bounds.height())
            height = minOf(metrics.bounds.width(), metrics.bounds.height())
            refreshRate = context.display?.refreshRate?.toDouble() ?: 60.0
        } else {
            @Suppress("DEPRECATION")
            val display = context.getSystemService(android.view.WindowManager::class.java).defaultDisplay
            @Suppress("DEPRECATION")
            val point = android.graphics.Point().also { display.getRealSize(it) }
            width = maxOf(point.x, point.y)
            height = minOf(point.x, point.y)
            @Suppress("DEPRECATION")
            run { refreshRate = display.refreshRate.toDouble() }
        }
        densityDpi = context.resources.displayMetrics.densityDpi
    }

    private fun loadCameras(context: Context) {
        cameras = try {
            val manager = context.getSystemService(CameraManager::class.java)
            manager.cameraIdList.mapNotNull { id ->
                val chars = manager.getCameraCharacteristics(id)
                val facing = when (chars.get(CameraCharacteristics.LENS_FACING)) {
                    CameraCharacteristics.LENS_FACING_BACK -> "back"
                    CameraCharacteristics.LENS_FACING_FRONT -> "front"
                    else -> "external"
                }
                val map = chars.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
                    ?: return@mapNotNull null
                val largest = largestSurfaceSize(map) ?: return@mapNotNull null
                Camera(id, facing, largest.width, largest.height)
            }
        } catch (e: Exception) {
            Log.e(TAG, "could not enumerate cameras", e)
            emptyList()
        }
    }

    private fun largestSurfaceSize(map: StreamConfigurationMap): android.util.Size? =
        map.getOutputSizes(android.graphics.ImageFormat.YUV_420_888)
            ?.maxByOrNull { it.width.toLong() * it.height }

    private const val TAG = "extraspace"
}
