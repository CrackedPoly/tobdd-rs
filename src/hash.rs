#[inline]
pub fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

#[inline]
pub fn splitmix64_2(x: u64, y: u64) -> u64 {
    let hx = splitmix64(x);
    let hy = splitmix64(y);
    hx ^ hy.rotate_left(1)
}

#[inline]
pub fn splitmix64_3(a: u64, b: u64, c: u64) -> u64 {
    let ha = splitmix64(a);
    let hb = splitmix64(b);
    let hc = splitmix64(c);

    let h = ha ^ hb.rotate_left(1) ^ hc.rotate_left(7);
    splitmix64(h)
}
