package io.github.tymonoman.extraspace

import java.io.DataInputStream
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Wire protocol, mirroring `xs-proto` on the host.
 *
 * Both sides must agree byte for byte. The magic number in every header means a
 * mismatch fails loudly on the first frame instead of quietly decoding garbage,
 * so if you change anything here, change `crates/xs-proto/src/lib.rs` too.
 */
object Protocol {
    /** Reads as the ASCII bytes `XSPA` little-endian. */
    const val MAGIC: Int = 0x41505358
    const val HEADER_LEN: Int = 20
    const val MAX_PAYLOAD: Int = 16 * 1024 * 1024
    const val VERSION: Int = 1

    object Channel {
        const val CONTROL: Byte = 0
        const val TOUCH: Byte = 1
        const val VIDEO_DOWN: Byte = 2
        const val CAMERA_UP: Byte = 3
    }

    object Flags {
        const val KEYFRAME: Short = 1
        const val CODEC_CONFIG: Short = 2
    }

    object ControlKind {
        const val HELLO: Byte = 0
        const val VIDEO_CONFIG: Byte = 1
        const val STATS: Byte = 2
        const val CAMERA_CONTROL: Byte = 3
        const val PING: Byte = 4
        const val PONG: Byte = 5
        const val ERROR: Byte = 6
    }

    object TouchAction {
        const val DOWN: Byte = 0
        const val MOTION: Byte = 1
        const val UP: Byte = 2
    }

    /** Abstract socket names; must match `sockets` in `xs-transport`. */
    object Sockets {
        const val CONTROL = "extraspace-control"
        const val VIDEO = "extraspace-video"
        const val CAMERA = "extraspace-camera"
    }
}

data class FrameHeader(
    val channel: Byte,
    val kind: Byte,
    val flags: Short,
    val length: Int,
    val ptsUs: Long,
)

class ProtocolException(message: String) : IOException(message)

/** Reads length-prefixed frames from a stream. Not thread-safe; one per socket. */
class FrameReader(input: InputStream) {
    private val stream = DataInputStream(input.buffered(64 * 1024))
    private val headerBuf = ByteArray(Protocol.HEADER_LEN)

    fun readHeader(): FrameHeader {
        stream.readFully(headerBuf)
        val bb = ByteBuffer.wrap(headerBuf).order(ByteOrder.LITTLE_ENDIAN)
        val magic = bb.int
        if (magic != Protocol.MAGIC) {
            throw ProtocolException(
                "bad magic 0x${magic.toUInt().toString(16)} - host/app protocol mismatch " +
                    "or a desynced stream"
            )
        }
        val channel = bb.get()
        val kind = bb.get()
        val flags = bb.short
        val length = bb.int
        val ptsUs = bb.long
        if (length < 0 || length > Protocol.MAX_PAYLOAD) {
            throw ProtocolException("payload of $length bytes is out of range")
        }
        return FrameHeader(channel, kind, flags, length, ptsUs)
    }

    /** Reads a payload into [dest], which must be at least [FrameHeader.length] long. */
    fun readPayload(header: FrameHeader, dest: ByteArray) {
        if (header.length > 0) stream.readFully(dest, 0, header.length)
    }

    fun readPayload(header: FrameHeader): ByteArray {
        val buf = ByteArray(header.length)
        readPayload(header, buf)
        return buf
    }
}

/** Writes frames to a stream. Synchronised: several producers share one socket. */
class FrameWriter(output: OutputStream) {
    private val stream = output.buffered(64 * 1024)
    private val headerBuf = ByteArray(Protocol.HEADER_LEN)

    @Synchronized
    fun write(channel: Byte, kind: Byte, flags: Short, ptsUs: Long, payload: ByteArray, length: Int = payload.size) {
        ByteBuffer.wrap(headerBuf).order(ByteOrder.LITTLE_ENDIAN).apply {
            putInt(Protocol.MAGIC)
            put(channel)
            put(kind)
            putShort(flags)
            putInt(length)
            putLong(ptsUs)
        }
        stream.write(headerBuf)
        if (length > 0) stream.write(payload, 0, length)
        stream.flush()
    }
}

/**
 * A single touch point. Fixed 21-byte encoding rather than JSON: these arrive at
 * up to 120 Hz per finger and parse cost is not free.
 */
data class TouchEvent(val action: Byte, val slot: Int, val x: Double, val y: Double) {
    fun encode(): ByteArray =
        ByteBuffer.allocate(TOUCH_PAYLOAD_LEN).order(ByteOrder.LITTLE_ENDIAN).apply {
            put(action)
            putInt(slot)
            putDouble(x)
            putDouble(y)
        }.array()

    companion object {
        const val TOUCH_PAYLOAD_LEN = 21
    }
}
