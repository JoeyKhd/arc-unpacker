using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

// Helpers for the installer comparison worker: file-cache flush, download completeness,
// folder fingerprints, and the text of any window a stuck installer shows.
public static class AbHelp
{
    [DllImport("ntdll.dll")] static extern int RtlAdjustPrivilege(int p, bool en, bool thread, out bool was);
    [DllImport("ntdll.dll")] static extern int NtSetSystemInformation(int cls, ref int info, int len);

    /// Flushes modified pages and purges the standby list (needs admin).
    public static string Purge()
    {
        bool was;
        RtlAdjustPrivilege(13, true, false, out was);       // SeProfileSingleProcessPrivilege
        int flush = 3, purge = 4;                            // MemoryFlushModifiedList, MemoryPurgeStandbyList
        return NtSetSystemInformation(80, ref flush, 4) + "/" + NtSetSystemInformation(80, ref purge, 4);
    }

    public static string Md5(string path)
    {
        using (var md5 = MD5.Create())
        using (var fs = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete, 1 << 20, FileOptions.SequentialScan))
            return BitConverter.ToString(md5.ComputeHash(fs)).Replace("-", "").ToLowerInvariant();
    }

    static List<KeyValuePair<string, string>> ReadList(string md5File)
    {
        var dir = Path.GetDirectoryName(md5File);
        var list = new List<KeyValuePair<string, string>>();
        foreach (var raw in File.ReadAllLines(md5File))
        {
            var line = raw.Trim().TrimStart('﻿');
            if (line.Length < 34 || line.StartsWith(";")) continue;
            var hash = line.Substring(0, 32).ToLowerInvariant();
            if (!hash.All(Uri.IsHexDigit)) continue;
            var name = line.Substring(32).TrimStart(' ', '*').Replace('/', '\\');
            list.Add(new KeyValuePair<string, string>(Path.GetFullPath(Path.Combine(dir, name)), hash));
        }
        return list;
    }

    /// Cheap pre-check: unfinished parts of a preallocated download read as zeros. Samples 64 KiB
    /// blocks across every listed file that exists; returns the first file with an all-zero block.
    public static string ZeroProbe(string md5File, int samplesPerFile)
    {
        var buf = new byte[65536];
        foreach (var e in ReadList(md5File))
        {
            if (!File.Exists(e.Key)) continue;
            using (var fs = new FileStream(e.Key, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete, 4096))
            {
                long len = fs.Length;
                if (len < buf.Length * 4) continue;
                for (int i = 0; i < samplesPerFile; i++)
                {
                    long pos = (len - buf.Length) / Math.Max(1, samplesPerFile - 1) * i;
                    fs.Position = pos;
                    int n = fs.Read(buf, 0, buf.Length);
                    bool zero = n == buf.Length;
                    for (int k = 0; zero && k < n; k += 8) if (BitConverter.ToInt64(buf, k) != 0) zero = false;
                    if (zero) return Path.GetFileName(e.Key);
                }
            }
        }
        return "";
    }

    /// Full check against the repack's MD5 list: "" when every listed file that exists matches
    /// (files that don't exist are optional parts that weren't downloaded).
    public static string CheckBins(string md5File)
    {
        var list = ReadList(md5File);
        var bad = new ConcurrentBag<string>();
        int present = 0;
        Parallel.ForEach(list, new ParallelOptions { MaxDegreeOfParallelism = 4 }, e =>
        {
            if (!File.Exists(e.Key)) return;
            Interlocked.Increment(ref present);
            if (Md5(e.Key) != e.Value) bad.Add(Path.GetFileName(e.Key));
        });
        if (present == 0) return "no listed file is present";
        return string.Join(", ", bad.OrderBy(x => x));
    }

    /// Writes "relative path <tab> size <tab> md5" for every file under root, sorted; returns the count.
    public static int Manifest(string root, string outFile)
    {
        root = root.TrimEnd('\\');
        var files = Directory.GetFiles(root, "*", SearchOption.AllDirectories);
        var lines = new string[files.Length];
        Parallel.For(0, files.Length, new ParallelOptions { MaxDegreeOfParallelism = 8 }, i =>
        {
            var f = files[i];
            lines[i] = f.Substring(root.Length + 1).ToLowerInvariant() + "\t" + new FileInfo(f).Length + "\t" + Md5(f);
        });
        Array.Sort(lines, StringComparer.Ordinal);
        File.WriteAllLines(outFile, lines);
        return files.Length;
    }

    delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] static extern bool EnumChildWindows(IntPtr h, EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);

    /// Titles and texts of the visible windows these processes own (to record why an installer stopped).
    public static string WindowText(int[] pids)
    {
        var sb = new StringBuilder();
        EnumWindows((h, l) =>
        {
            uint pid;
            GetWindowThreadProcessId(h, out pid);
            if (Array.IndexOf(pids, (int)pid) < 0 || !IsWindowVisible(h)) return true;
            var t = new StringBuilder(512);
            GetWindowText(h, t, 512);
            sb.Append("[").Append(t).Append("] ");
            EnumChildWindows(h, (c, l2) =>
            {
                var ct = new StringBuilder(2048);
                GetWindowText(c, ct, 2048);
                if (ct.Length > 0) sb.Append(ct).Append(" | ");
                return true;
            }, IntPtr.Zero);
            return true;
        }, IntPtr.Zero);
        return sb.ToString();
    }
}
