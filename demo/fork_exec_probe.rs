// M5.2 D8f 永久差分探针：guest 单线程 fork 直通 + exec 族直通，与 native 对拍。
// pid 非确定不打印；child 退出码/计算/exec 输出确定。覆盖 ①fork + child 做 heap
// 分配 + exit ②fork + child execvp（进程替换）。
use std::ffi::CString;

unsafe extern "C" {
    #[link_name = "fork"]
    fn c_fork() -> i32;
    fn waitpid(pid: i32, status: *mut i32, opts: i32) -> i32;
    fn execvp(file: *const i8, argv: *const *const i8) -> i32;
    #[link_name = "_exit"]
    fn c_exit(code: i32) -> !;
}

fn main() {
    // ① fork + child heap work + exit code
    let pid = unsafe { c_fork() };
    if pid == 0 {
        let v: Vec<u64> = (0..500).map(|i| i * 3).collect();
        let s: u64 = v.iter().sum();
        unsafe { c_exit((s % 100) as i32) };
    }
    let mut status = 0i32;
    unsafe { waitpid(pid, &mut status, 0) };
    println!("child1 exit = {}", (status >> 8) & 0xff);

    // ② fork + execvp（exec 族直通：进程替换，VM 状态消失本就正确）
    let pid2 = unsafe { c_fork() };
    if pid2 == 0 {
        let prog = CString::new("/bin/echo").unwrap();
        let arg = CString::new("exec-child-ok").unwrap();
        let argv: [*const i8; 3] = [prog.as_ptr(), arg.as_ptr(), std::ptr::null()];
        unsafe { execvp(prog.as_ptr(), argv.as_ptr()) };
        unsafe { c_exit(127) }; // execvp 失败才到这
    }
    let mut status2 = 0i32;
    unsafe { waitpid(pid2, &mut status2, 0) };
    println!("child2 (exec) exit = {}", (status2 >> 8) & 0xff);
}
