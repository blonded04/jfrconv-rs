/* Test transport adapter. Copyright OpenAI. SPDX-License-Identifier: Apache-2.0 */
package one.convert;

import java.io.*;
import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;
import java.util.regex.Pattern;

/** Routes published Java converter entry points to a native CLI; assertions stay untouched. */
public final class RustBridge {
    private static String binary() throws IOException {
        String value = System.getenv("JFRCONV_BINARY");
        if (value == null || value.isEmpty()) throw new IOException("JFRCONV_BINARY is not set");
        return value;
    }
    public static int execute(String... args) throws IOException {
        List<String> command = new ArrayList<>(); command.add(binary());
        command.addAll(Arrays.asList(args));
        try {
            return new ProcessBuilder(command).inheritIO().start().waitFor();
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt(); throw new IOException(e);
        }
    }
    public static void main(String... args) throws IOException {
        int status = execute(args);
        if (status != 0) System.exit(status);
    }
    public static void convert(String input, String output, Arguments args, String format) throws IOException {
        List<String> cli = new ArrayList<>();
        Arguments defaults = new Arguments();
        for (Field field : Arguments.class.getFields()) {
            if (Modifier.isStatic(field.getModifiers()) || Modifier.isFinal(field.getModifiers()) || "output".equals(field.getName())) continue;
            try {
                Object value = field.get(args);
                if (value == null || Objects.equals(value, field.get(defaults))) continue;
                String name = "cpuTime".equals(field.getName()) ? "cpu-time" : field.getName();
                if (value instanceof Boolean) {
                    if ((Boolean) value) cli.add("--" + name);
                } else if (value instanceof String || value instanceof Number || value instanceof Pattern) {
                    cli.add("--" + name);
                    // Java Arguments stores latency in ns; native CLI accepts ms.
                    cli.add("latency".equals(name) && value instanceof Number
                            ? Double.toString(((Number) value).doubleValue() / 1_000_000.0)
                            : value.toString());
                }
            } catch (IllegalAccessException e) { throw new IOException(e); }
        }
        cli.add("-o"); cli.add(format); cli.add(input); cli.add(output);
        int status = execute(cli.toArray(new String[0]));
        if (status != 0) throw new IOException("Native converter exited " + status + ": " + cli);
    }
    public static byte[] collapsed(String input, String... flags) throws IOException {
        Path output = Files.createTempFile("jfrconv-public-", ".collapsed");
        try {
            convert(input, output.toString(), new Arguments(flags), "collapsed");
            return Files.readAllBytes(output);
        } finally { Files.deleteIfExists(output); }
    }
}
