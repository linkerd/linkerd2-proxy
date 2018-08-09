use std::fmt;

/// Represents a platform-agnostic system error for metrics labels.
// This exists to hide the `enum` vs `struct` difference of the
// platform-specific type.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Errno(self::code::Code);

impl<C: Into<self::code::Code>> From<C> for Errno {
    fn from(c: C) -> Self {
        Errno(c.into())
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(not(target_os="windows"))]
mod code {
    use std::fmt;

    macro_rules! mk_err_enum {
        { $(#[$m:meta])* enum $name:ident from $from_ty:ty {
            $( $from:pat => $reason:ident ),+
        } } => {
            $(#[$m])*
            pub enum $name {
                $( $reason ),+
            }

            impl fmt::Display for $name {
                fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    match self {
                        $(
                            $name::$reason => f.pad(stringify!($reason))
                        ),+
                    }
                }
            }

            impl<'a> From<$from_ty> for $name {
                fn from(err: $from_ty) -> Self {
                    match err {
                        $(
                            $from => $name::$reason
                        ),+
                    }
                }
            }
        }
    }

    mk_err_enum! {
        /// Taken from `errno.h`.
        #[allow(non_camel_case_types)]
        #[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
        enum Code from i32 {
            1 => EPERM,
            2 => ENOENT,
            3 => ESRCH,
            4 => EINTR,             // Interrupted system call
            5 => EIO,               // I/O error
            6 => ENXIO,             // No such device or address
            7 => E2BIG,             // Argument list too long
            8 => ENOEXEC,           // Exec format error
            9 => EBADF,             // Bad file number
            10 => ECHILD,           // No child processes
            11 => EAGAIN,           // Try again
            12 => ENOMEM,           // Out of memory
            13 => EACCES,           // Permission denied
            14 => EFAULT,           // Bad address
            15 => ENOTBLK,          // Block device required
            16 => EBUSY,            // Device or resource busy
            17 => EEXIST,           // File exists
            18 => EXDEV,            // Cross-device linkgit pus
            19 => ENODEV,           // No such device
            20 => ENOTDIR,          // Not a directory
            21 => EISDIR,           // Is a directory
            22 => EINVAL,           // Invalid argument
            23 => ENFILE,           // File table overflow
            24 => EMFILE,           // Too many open files
            25 => ENOTTY,           // Not a typewriter
            26 => ETXTBSY,          // Text file busy
            27 => EFBIG,            // File too large
            28 => ENOSPC,           // No space left on device
            29 => ESPIPE,           // Illegal seek
            30 => EROFS,            // Read-only file system
            31 => EMLINK,           // Too many links
            32 => EPIPE,            // Broken pipe
            33 => EDOM,             // Math argument out of domain of func
            34 => ERANGE,           // Math result not representable
            35 => EDEADLK,          // Resource deadlock would occur
            36  => ENAMETOOLONG,    // File name too long
            37  => ENOLCK,          // No record locks available
            38  => ENOSYS,          // Function not implemented
            39  => ENOTEMPTY,       // Directory not empty
            40  => ELOOP,           // Too many symbolic links encountered
            42  => ENOMSG,          // No message of desired type
            43  => EIDRM,           // Identifier removed
            44  => ECHRNG,          // Channel number out of range
            45  => EL2NSYNC,        // Level 2 not synchronized
            46  => EL3HLT,          // Level 3 halted
            47  => EL3RST,          // Level 3 reset
            48  => ELNRNG,          // Link number out of range
            49  => EUNATCH,         // Protocol driver not attached
            50  => ENOCSI,          // No CSI structure available
            51  => EL2HLT,          // Level 2 halted
            52  => EBADE,           // Invalid exchange
            53  => EBADR,           // Invalid request descriptor
            54  => EXFULL,          // Exchange full
            55  => ENOANO,          // No anode
            56  => EBADRQC,         // Invalid request code
            57  => EBADSLT,         // Invalid slot
            59  => EBFONT,          // Bad font file format
            60  => ENOSTR,          // Device not a stream
            61  => ENODATA,         // No data available
            62  => ETIME,           // Timer expired
            63  => ENOSR,           // Out of streams resources
            64  => ENONET,          // Machine is not on the network
            65  => ENOPKG,          // Package not installed
            66  => EREMOTE,         // Object is remote
            67  => ENOLINK,         // Link has been severed
            68  => EADV,            // Advertise error
            69  => ESRMNT,          // Srmount error
            70  => ECOMM,           // Communication error on send
            71  => EPROTO,          // Protocol error
            72  => EMULTIHOP,       // Multihop attempted
            73  => EDOTDOT,         // RFS specific error
            74  => EBADMSG,         // Not a data message
            75  => EOVERFLOW,       // Value too large for defined data type
            76  => ENOTUNIQ,        // Name not unique on network
            77  => EBADFD,          // File descriptor in bad state
            78  => EREMCHG,         // Remote address changed
            79  => ELIBACC,         // Can not access a needed shared library
            80  => ELIBBAD,         // Accessing a corrupted shared library
            81  => ELIBSCN,         // .lib section in a.out corrupted
            82  => ELIBMAX,         // Attempting to link in too many shared libraries
            83  => ELIBEXEC,        // Cannot exec a shared library directly
            84  => EILSEQ,          // Illegal byte sequence
            85  => ERESTART,        // Interrupted system call should be restarted
            86  => ESTRPIPE,        // Streams pipe error
            87  => EUSERS,          // Too many users
            88  => ENOTSOCK,        // Socket operation on non-socket
            89  => EDESTADDRREQ,    // Destination address required
            90  => EMSGSIZE,        // Message too long
            91  => EPROTOTYPE,      // Protocol wrong type for socket
            92  => ENOPROTOOPT,     // Protocol not available
            93  => EPROTONOSUPPORT, // Protocol not supported
            94  => ESOCKTNOSUPPORT, // Socket type not supported
            95  => EOPNOTSUPP,      // Operation not supported on transport endpoint
            96  => EPFNOSUPPORT,    // Protocol family not supported
            97  => EAFNOSUPPORT,    // Address family not supported by protocol
            98  => EADDRINUSE,      // Address already in use
            99  => EADDRNOTAVAIL,   // Cannot assign requested address
            100 => ENETDOWN,        // Network is down
            101 => ENETUNREACH,     // Network is unreachable
            102 => ENETRESET,       // Network dropped connection because of reset
            103 => ECONNABORTED,    // Software caused connection abort
            104 => ECONNRESET,      // Connection reset by peer
            105 => ENOBUFS,         // No buffer space available
            106 => EISCONN,         // Transport endpoint is already connected
            107 => ENOTCONN,        // Transport endpoint is not connected
            108 => ESHUTDOWN,       // Cannot send after transport endpoint shutdown
            109 => ETOOMANYREFS,    // Too many references: cannot splice
            110 => ETIMEDOUT,       // Connection timed out
            111 => ECONNREFUSED,    // Connection refused
            112 => EHOSTDOWN,       // Host is down
            113 => EHOSTUNREACH,    // No route to host
            114 => EALREADY,        // Operation already in progress
            115 => EINPROGRESS,     // Operation now in progress
            116 => ESTALE,          // Stale NFS file handle
            117 => EUCLEAN,         // Structure needs cleaning
            118 => ENOTNAM,         // Not a XENIX named type file
            119 => ENAVAIL,         // No XENIX semaphores available
            120 => EISNAM,          // Is a named type file
            121 => EREMOTEIO,       // Remote I/O error
            122 => EDQUOT,          // Quota exceeded
            123 => ENOMEDIUM,       // No medium found
            124 => EMEDIUMTYPE,     // Wrong medium type
            125 => ECANCELED,       // Operation Canceled
            126 => ENOKEY,          // Required key not available
            127 => EKEYEXPIRED,     // Key has expired
            128 => EKEYREVOKED,     // Key has been revoked
            129 => EKEYREJECTED,    // Key was rejected by service
            130 => EOWNERDEAD,      // Owner died
            131 => ENOTRECOVERABLE, // State not recoverable
            _   => UNKNOWN_ERRNO
        }
    }
}

#[cfg(target_os="windows")]
mod code {
    use std::fmt;

    #[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
    pub struct Code(i32);

    #[cfg(target_os="windows")]
    impl From<i32> for Code {
        fn from(n: i32) -> Self {
            Errno(codes::Code(n))
        }
    }

    impl fmt::Display for Code {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            fmt::Display.fmt(self.0, f)
        }
    }
}
