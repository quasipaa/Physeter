use super::chunk::{Chunk, Codec, LazyResult};
use super::{fs::Fs, KernelOptions};
use bytes::{Buf, BufMut, Bytes};
use anyhow::Result;

/// 存储轨道
///
/// 数据存储在轨道文件内，
/// 数据被拆分成固定大小的分片以链表形式写入，
/// 删除数据只会标记分片为失效，下次写入将覆盖分片
///
/// `options` 配置  
/// `free_start` 失效头索引  
/// `free_end` 失效尾部索引  
/// `chunk` 分片类  
/// `size` 轨道大小  
/// `file` 文件类  
/// `id` 轨道ID
pub struct Track<'a> {
    options: &'a KernelOptions<'a>,
    free_start: u64,
    free_end: u64,
    chunk: Codec,
    pub size: u64,
    file: Fs,
    id: u16,
}

impl<'a> Track<'a> {
    /// 创建轨道
    ///
    /// ```no_run
    /// use super::{Track, KernelOptions};
    ///
    /// let options = KernelOptions::default();
    /// let track = Track::new(0, &options);
    /// ```
    pub async fn new(id: u16, options: &'a KernelOptions<'_>) -> Result<Track<'a>> {
        let path = options.directory.join(format!("{}.track", id));
        Ok(Self {
            file: Fs::new(path.as_path()).await?,
            chunk: Codec::new(options),
            free_start: 0,
            free_end: 0,
            size: 0,
            options,
            id,
        })
    }

    /// 初始化
    ///
    /// 必须对该实例调用初始化，
    /// 才能进行其他操作
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use super::{Track, KernelOptions};
    ///
    /// let options = KernelOptions::default();
    /// let mut track = Track::new(0, &options);
    /// track.init().await?;
    /// ```
    pub async fn init(&mut self) -> Result<()> {
        self.size = self.file.stat().await?.len();
        self.read_header().await
    }

    /// 读取分片数据
    ///
    /// 读取单个分片数据
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use super::{Track, KernelOptions};
    ///
    /// let options = KernelOptions::default();
    /// let mut track = Track::new(0, &options);
    /// track.init().await?;
    /// let chunk = track.read(10).await?;
    /// ```
    pub async fn read(&mut self, offset: u64) -> Result<Chunk> {
        let mut packet = vec![0u8; self.options.track_size as usize];
        self.file.read(&mut packet, offset).await?;
        Ok(self.chunk.decoder(Bytes::from(packet)))
    }

    /// 分配分片写入位置
    ///
    /// 因为链表的特殊性，
    /// 所以这个地方并不直接写入数据，
    /// 而是预先分配位置
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use super::{Track, KernelOptions};
    ///
    /// let options = KernelOptions::default();
    /// let mut track = Track::new(0, &options);
    /// track.init().await?;
    /// let index = track.alloc().await?;
    /// ```
    pub async fn alloc(&mut self) -> Result<u64> {
        // 没有失效块
        // 直接写入轨道尾部
        if self.free_start == 0 {
            let index = self.size;
            self.size += self.options.chunk_size;
            return Ok(index);
        }

        // 读取失效分片
        // 并解码失效分片
        let mut buffer = vec![0u8; self.options.chunk_size as usize];
        self.file.read(&mut buffer, self.free_start).await?;
        let value = self.chunk.lazy_decoder(Bytes::from(buffer));

        // 如果还有失效分片
        // 则更新链表头部为下个分片位置
        // 如果失效分片已经全部解决
        // 则归零链表头部
        let free_start = self.free_start;
        self.free_start = match value.next {
            Some(next) => next,
            None => 0,
        };

        // 归零链表头部时
        // 也同时归零链表尾部
        // 因为已无失效分片
        if self.free_start == 0 {
            self.free_end = 0
        }

        Ok(free_start)
    }

    /// 删除数据
    ///
    /// 和其他函数不同，
    /// 因为删除是个需要连续性的操作，
    /// 所以这里只用给定头部分片，
    /// 内部将一直根据链表索引删除下去，
    /// 当遇到跳出当前轨道去往其他轨道的时候，
    /// 将返回其他轨道的ID
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use super::{Track, KernelOptions};
    ///
    /// let options = KernelOptions::default();
    /// let mut track = Track::new(0, &options);
    /// track.init().await?;
    /// let track_id = track.remove(10).await?;
    /// ```
    #[rustfmt::skip]
    pub async fn remove(&mut self, index: u64) -> Result<Option<LazyResult>> {
        let mut first = false;
        let mut offset = index;
        let free_byte = vec![0u8];

        // 无限循环
        // 直到失效所有分片
    loop {

        // 遍历完文件
        // 跳出循环
        if offset >= self.options.track_size {
            break;
        }

        // 读取分片
        // 如果没有数据则跳出
        let mut chunk = vec![0u8; self.options.chunk_size as usize];
        let size = self.file.read(&mut chunk[..], offset).await?;
        if size == 0 {
            break;
        }

        // 轨道数据长度减去单分片长度
        // 更改状态位为失效并解码当前分片
        self.size -= self.options.chunk_size;
        self.file.write(&free_byte, offset + 4).await?;
        let value = self.chunk.lazy_decoder(Bytes::from(chunk));

        // 如果失效索引头未初始化
        // 则先初始化索引头
        if self.free_start == 0 {
            let mut next_buf = vec![0u8; 8];
            next_buf.put_u64(offset);
            self.file.write(&next_buf, 0).await?;
            self.free_start = offset;
        }

        // 如果尾部索引已存在
        // 则将当前尾部和现在的分片索引连接
        // 连接的目的是因为失效块是个连续的链表
        // 所以这里将首个失效块跟上个尾部失效块连接
        if self.free_end > 0 && first == false {
            let mut next_buf = vec![0u8; 8];
            next_buf.put_u64(offset);
            self.file.write(&next_buf, self.free_end + 7).await?;
        }

        // 如果下个索引为空
        // 则表示分片列表已到尾部
        // 更新失效索引尾部并跳出循环
        if let None = value.next {
            let mut end_buf = vec![0u8; 8];
            end_buf.put_u64(offset);
            self.file.write(&end_buf, 8).await?;
            self.free_end = offset;
            break;
        }

        // 更新索引
        if let Some(next) = value.next {
            offset = next;
        }

        // 下个索引不在这个轨道文件
        // 转移到其他轨道继续流程
        first = true;
        if let Some(track) = value.next_track {
            if track != self.id {
                return Ok(Some(value))
            }
        }
    }

        Ok(None)
    }

    /// 写入分片
    ///
    /// 写入单个分片数据到磁盘文件
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use super::{Track, Chunk, KernelOptions};
    ///
    /// let chunk = Chunk {
    ///     id: 0,
    ///     exist: true,
    ///     next: Some(17),
    ///     next_track: None,
    ///     data: Bytes::from_static(b"hello"),
    /// };
    ///
    /// let options = KernelOptions::default();
    /// let mut track = Track::new(0, &options);
    /// track.init().await?;
    /// track.write(Chunk, 20).await?;
    /// ```
    pub async fn write(&mut self, chunk: Chunk, index: u64) -> Result<()> {
        self.file.write(&self.chunk.encoder(chunk), index).await
    }

    /// 写入结束
    ///
    /// 当数据流写入完成的时候，
    /// 将状态同步到磁盘文件，
    /// 这是一个必要的操作，
    /// 但是不强制什么时候调用，
    /// 不过一定要在关闭实例之前调用一次
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use super::{Track, Chunk, KernelOptions};
    ///
    /// let chunk = Chunk {
    ///     id: 0,
    ///     exist: true,
    ///     next: Some(17),
    ///     next_track: None,
    ///     data: Bytes::from_static(b"hello"),
    /// };
    ///
    /// let options = KernelOptions::default();
    /// let mut track = Track::new(0, &options);
    /// track.init().await?;
    /// track.write(Chunk, 20).await?;
    /// track.write_end().await?;
    /// ```
    pub async fn write_end(&mut self) -> Result<()> {
        let mut packet = vec![0u8; 16];
        packet.put_u64(self.free_start);
        packet.put_u64(self.free_end);
        self.file.write(&packet, 0).await
    }

    /// 创建默认文件头
    ///
    /// 将默认的失效块头索引和尾部索引写入到磁盘文件,
    /// 并初始化文件长度状态
    async fn default_header(&mut self) -> Result<()> {
        self.file.write(&vec![0, 8], 0).await?;
        self.size = 16;
        Ok(())
    }

    /// 读取文件头
    ///
    /// 从磁盘文件中读取失效块头索引和尾部索引，
    /// 这是必要的操作，轨道实例化的时候必须要
    /// 从文件中恢复上次的状态
    async fn read_header(&mut self) -> Result<()> {
        // 如果文件为空
        // 则直接写入默认头索引
        if self.size == 0 {
            return self.default_header().await;
        }

        // 从文件中读取头部
        let mut buffer = [0u8; 16];
        self.file.read(&mut buffer, 0).await?;
        let mut packet = Bytes::from(buffer.to_vec());

        // 将状态同步到实例内部
        self.free_start = packet.get_u64();
        self.free_end = packet.get_u64();

        Ok(())
    }
}