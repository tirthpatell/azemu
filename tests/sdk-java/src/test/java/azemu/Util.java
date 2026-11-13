package azemu;

import java.io.*;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;
import java.util.function.Supplier;

final class Util {
    private Util() {}

    static byte[] bytes(int n, long seed) {
        byte[] b = new byte[n];
        new Random(seed).nextBytes(b);
        return b;
    }

    static byte[] bytes(int n) { return bytes(n, n); }

    static String sha256(byte[] b) {
        try { return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(b)); }
        catch (Exception e) { throw new RuntimeException(e); }
    }

    static String sha256(Path p) throws IOException {
        try (InputStream in = Files.newInputStream(p)) {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            byte[] buf = new byte[1 << 16];
            int r;
            while ((r = in.read(buf)) > 0) md.update(buf, 0, r);
            return HexFormat.of().formatHex(md.digest());
        } catch (java.security.NoSuchAlgorithmException e) { throw new RuntimeException(e); }
    }

    static Path randomFile(long size, long seed) throws IOException {
        Path p = Files.createTempFile("azemu-java", ".bin");
        p.toFile().deleteOnExit();
        Random r = new Random(seed);
        byte[] buf = new byte[1 << 16];
        try (OutputStream out = Files.newOutputStream(p)) {
            long left = size;
            while (left > 0) {
                r.nextBytes(buf);
                int n = (int) Math.min(buf.length, left);
                out.write(buf, 0, n);
                left -= n;
            }
        }
        return p;
    }

    /** Polls until the supplier returns non-null or the time is up. */
    static <T> T eventually(long millis, Supplier<T> s) throws InterruptedException {
        long end = System.currentTimeMillis() + millis;
        T v = null;
        while (System.currentTimeMillis() < end) {
            v = s.get();
            if (v != null) return v;
            Thread.sleep(100);
        }
        return v;
    }
}
