package azemu;

import java.io.IOException;
import java.net.ServerSocket;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.file.*;
import java.security.KeyStore;
import java.security.cert.Certificate;
import java.security.cert.CertificateFactory;
import java.time.Duration;
import java.util.*;
import java.util.concurrent.atomic.AtomicLong;

/**
 * One running emulator: a free port, a temp certificate dir shared by all emulators of this JVM, debug log in
 * target/emu-logs. TLS: the first start builds a truststore (the JDK's cacerts plus the emulator's ca.pem) and sets
 * the JVM-wide system property javax.net.ssl.trustStore before any client exists. SSL_CERT_FILE is never touched.
 */
public final class Emu implements AutoCloseable {
    public static final String DEV_ACCOUNT = "devstoreaccount1";
    public static final String DEV_KEY =
        "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

    static {
        // temp files (64 MB uploads, certificates) go to target/tmp on the disk: /tmp is a small tmpfs shared with other jobs
        try { Files.createDirectories(Paths.get(System.getProperty("java.io.tmpdir"))); } catch (IOException e) { throw new ExceptionInInitializerError(e); }
    }

    private static Path certDir;
    private static Path trustStore;
    private static final AtomicLong counter = new AtomicLong(System.nanoTime() % 100000);
    private static final String RUN = Long.toString(System.currentTimeMillis() % 1_000_000L, 36);
    private static Emu current;

    public final int port;
    private final Process proc;
    public final Path log;

    private Emu(int port, Process proc, Path log) {
        this.port = port;
        this.proc = proc;
        this.log = log;
    }

    public static Emu current() {
        if (current == null) throw new IllegalStateException("no emulator running");
        return current;
    }

    static Path binary() {
        String env = System.getenv("AZEMU_BIN");
        Path p = env != null && !env.isEmpty() ? Paths.get(env) : Paths.get(".azemu-under-test");
        if (!Files.isExecutable(p)) throw new IllegalStateException("emulator binary not found: " + p.toAbsolutePath());
        return p.toAbsolutePath();
    }

    public static synchronized Emu start(String name, Map<String, String> env) throws Exception {
        if (certDir == null) certDir = Files.createTempDirectory("azemu-java-certs");
        int port;
        try (ServerSocket s = new ServerSocket(0)) { port = s.getLocalPort(); }
        Path logDir = Paths.get("target", "emu-logs");
        Files.createDirectories(logDir);
        Path log = logDir.resolve(name + ".log");
        ProcessBuilder pb = new ProcessBuilder(binary().toString(), "--port", Integer.toString(port),
            "--cert-dir", certDir.toString(), "--log-level", "debug");
        pb.environment().putAll(env);
        pb.redirectErrorStream(true);
        pb.redirectOutput(log.toFile());
        Process proc = pb.start();
        Emu emu = new Emu(port, proc, log);
        try {
            emu.waitHealthy();
            if (trustStore == null) buildTrustStore();
            else ensureSameCa();
        } catch (Exception e) {
            proc.destroyForcibly();
            throw e;
        }
        Runtime.getRuntime().addShutdownHook(new Thread(proc::destroyForcibly));
        current = emu;
        return emu;
    }

    /**
     * Plain HttpURLConnection on purpose: java.net.http.HttpClient creates SSLContext.getDefault() in its constructor, and
     * that default (cacerts only) would then be used by clients that read it later (qpid-proton-j's TLS for AMQP over wss),
     * before the trust store property below is set.
     */
    private void waitHealthy() throws Exception {
        long deadline = System.currentTimeMillis() + 30_000;
        Exception last = null;
        while (System.currentTimeMillis() < deadline) {
            if (!proc.isAlive()) throw new IllegalStateException("emulator exited early: " + proc.exitValue());
            try {
                java.net.HttpURLConnection c = (java.net.HttpURLConnection) new URI(gateway() + "/_azemu/health").toURL().openConnection();
                c.setConnectTimeout(1000);
                c.setReadTimeout(1000);
                int code = c.getResponseCode();
                c.disconnect();
                if (code == 200) return;
            } catch (Exception e) { last = e; }
            Thread.sleep(100);
        }
        throw new IllegalStateException("emulator not healthy on " + port, last);
    }

    private static void buildTrustStore() throws Exception {
        KeyStore ks = KeyStore.getInstance(KeyStore.getDefaultType());
        Path cacerts = Paths.get(System.getProperty("java.home"), "lib", "security", "cacerts");
        try (var in = Files.newInputStream(cacerts)) { ks.load(in, "changeit".toCharArray()); }
        try (var in = Files.newInputStream(certDir.resolve("ca.pem"))) {
            for (Certificate c : CertificateFactory.getInstance("X.509").generateCertificates(in)) {
                ks.setCertificateEntry("azemu-" + ks.size(), c);
            }
        }
        String extra = System.getenv("AZEMU_EXTRA_TRUST_PEM"); // development aid, see Cos.endpoint
        if (extra != null && !extra.isEmpty()) {
            try (var in = Files.newInputStream(Paths.get(extra))) {
                for (Certificate c : CertificateFactory.getInstance("X.509").generateCertificates(in)) ks.setCertificateEntry("extra-" + ks.size(), c);
            }
        }
        trustStore = certDir.resolve("truststore.p12");
        try (var out = Files.newOutputStream(trustStore)) { ks.store(out, "changeit".toCharArray()); }
        System.setProperty("javax.net.ssl.trustStore", trustStore.toString());
        System.setProperty("javax.net.ssl.trustStorePassword", "changeit");
        System.setProperty("javax.net.ssl.trustStoreType", "PKCS12");
    }

    private static String caFingerprint;
    private static void ensureSameCa() throws Exception {
        // every emulator of this JVM uses the same cert dir, so the CA must be the same; fail loudly if not
        String now = new String(Files.readAllBytes(certDir.resolve("ca.pem")));
        if (caFingerprint == null) caFingerprint = now;
        else if (!caFingerprint.equals(now)) throw new IllegalStateException("ca.pem changed between emulator starts");
    }

    public static Path caPem() { return certDir.resolve("ca.pem"); }

    // ---- URLs --------------------------------------------------------------------------------------------
    public String gateway() { return "http://127.0.0.1:" + port; }
    public String host(String label, String name) {
        String suffix = switch (label) { case "blob", "queue", "table", "dfs" -> "core.localhost"; default -> "localhost"; };
        return name + "." + label + "." + suffix + ":" + port;
    }
    public String url(String label, String name) { return "http://" + host(label, name); }
    public String https(String label, String name) { return "https://" + host(label, name); }
    public String blobUrl(String acct) { return url("blob", acct); }
    public String dfsUrl(String acct) { return url("dfs", acct); }
    public String queueUrl(String acct) { return url("queue", acct); }
    public String tableUrl(String acct) { return url("table", acct); }
    public String vaultUrl(String v) { return https("vault", v); }
    public String appConfigUrl(String s) { return https("azconfig", s); }
    public String cosmosUrl(String a) { return https("documents", a); }
    public String armUrl() { return "https://management.localhost:" + port; }

    public String storageConnStr(String account, String key) {
        return "DefaultEndpointsProtocol=http;AccountName=" + account + ";AccountKey=" + key
            + ";BlobEndpoint=" + blobUrl(account) + ";QueueEndpoint=" + queueUrl(account)
            + ";TableEndpoint=" + tableUrl(account) + ";";
    }
    public String storageConnStr(String account) { return storageConnStr(account, DEV_KEY); }

    public String sbConnStr(String ns) {
        return "Endpoint=sb://" + ns + ".servicebus.localhost:" + port
            + ";SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=any;UseDevelopmentEmulator=true";
    }
    public String ehConnStr(String ns, String hub) {
        return "Endpoint=sb://" + ns + ".eventhub.localhost:" + port
            + ";SharedAccessKeyName=RootManageSharedAccessKey;SharedAccessKey=any;UseDevelopmentEmulator=true"
            + (hub == null ? "" : ";EntityPath=" + hub);
    }

    // ---- names -------------------------------------------------------------------------------------------
    /** Unique lower-case name, letters and digits only (storage account / container safe). */
    public static String name(String prefix) {
        return (prefix + RUN + Long.toString(counter.incrementAndGet(), 36)).toLowerCase(Locale.ROOT).replaceAll("[^a-z0-9]", "");
    }
    public static String dashed(String prefix) {
        return (prefix + "-" + RUN + "-" + Long.toString(counter.incrementAndGet(), 36)).toLowerCase(Locale.ROOT);
    }

    // ---- admin -------------------------------------------------------------------------------------------
    public String get(String path) throws Exception {
        HttpResponse<String> r = HttpClient.newHttpClient().send(
            HttpRequest.newBuilder(URI.create(gateway() + path)).build(), HttpResponse.BodyHandlers.ofString());
        return r.body();
    }

    public void reset() throws Exception {
        HttpClient.newHttpClient().send(HttpRequest.newBuilder(URI.create(gateway() + "/_azemu/reset"))
            .POST(HttpRequest.BodyPublishers.noBody()).build(), HttpResponse.BodyHandlers.discarding());
    }

    @Override
    public void close() {
        proc.destroy();
        try {
            if (!proc.waitFor(5, java.util.concurrent.TimeUnit.SECONDS)) proc.destroyForcibly();
        } catch (InterruptedException e) {
            proc.destroyForcibly();
        }
        if (current == this) current = null;
    }
}
