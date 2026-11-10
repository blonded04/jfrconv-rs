/* Test workload: JDK JFR or async-profiler may record this process. */
import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import jdk.jfr.Recording;

public final class RecordingWorkload {
    static volatile long sink;
    static volatile Object allocation;
    static final Object monitor = new Object();

    static void cpuWork(long end) {
        long x = 17;
        while (System.nanoTime() < end) {
            for (int i = 0; i < 2000; ++i) x = (x * 6364136223846793005L + 1) ^ (x >>> 13);
            allocation = new byte[1024 + (int) (x & 4095)];
            synchronized (monitor) { sink = x; }
        }
    }

    public static void main(String[] args) throws Exception {
        String kind = args.length > 1 ? args[1] : "jdk";
        Recording recording = null;
        if (kind.equals("jdk")) {
            recording = new Recording();
            recording.enable("jdk.ExecutionSample").withPeriod(Duration.ofMillis(1));
            recording.enable("jdk.NativeMethodSample").withPeriod(Duration.ofMillis(1));
            recording.enable("jdk.ObjectAllocationSample").with("throttle", "1000/s");
            recording.enable("jdk.JavaMonitorEnter").withThreshold(Duration.ZERO);
            recording.enable("jdk.ThreadPark").withThreshold(Duration.ZERO);
            recording.start();
        }
        long end = System.nanoTime() + 2_000_000_000L;
        List<Thread> workers = new ArrayList<>();
        for (int i = 0; i < 3; i++) {
            Thread t = new Thread(() -> cpuWork(end), "work-" + i);
            workers.add(t); t.start();
        }
        for (Thread t : workers) t.join();
        if (recording != null) {
            recording.stop(); recording.dump(Path.of(args[0])); recording.close();
        }
        System.out.println(sink);
    }
}
